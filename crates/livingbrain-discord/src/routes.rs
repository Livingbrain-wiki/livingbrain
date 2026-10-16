//! The routes under `/v1/discord`. Every one is for a workspace owner or admin,
//! the same guard as GitHub's settings routes.
//!
//! | route | what |
//! |---|---|
//! | `GET /` | availability, the connected servers, their channels and import progress |
//! | `GET /connect` | a `302` to Discord's bot-install page, carrying a signed, single-use `state` |
//! | `GET /callback?guild_id&state` | Discord's return. Spends the state, confirms with Discord that the bot is in the server, records it |
//! | `DELETE /{guild_id}` | disconnects a server: its channels stop being read at once |
//! | `PATCH /{guild_id}/channels/{channel_id}` | `{included}`: import this channel or not |
//!
//! There is no route that reads history or files anything: including a
//! channel makes it due, and the next cron tick does the reading.
//!
//! The callback is a browser navigation from discord.com, so it carries the
//! session cookie (`SameSite=Lax`) and is guarded by its single-use state,
//! which is bound to the member who minted it. It answers with a redirect to a
//! fixed settings path, never a URL read from the request. Discord's
//! `guild_id` is trusted only after the state is spent and Discord confirms the
//! bot is in that server.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Path, Query, State};
use cratefield_core::axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{delete, get, patch};
use cratefield_core::axum::{self, Json};
use cratefield_core::{Database, HttpClient, ModuleContext, Problem, ProblemDef};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{self, Guild};
use crate::common::{StoreError, is_snowflake};
use crate::config::DiscordApp;
use crate::state;
use crate::store::{self, Channel, ConnectOutcome, Connection};

pub(crate) const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "discord/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Owner or admin access required",
    description: "Only a workspace owner or admin can connect or change Discord.",
};

pub(crate) const NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "discord/not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Discord is not available here",
    description: "The Discord app's secrets are not set on this deployment.",
};

pub(crate) const NOT_FOUND: ProblemDef = ProblemDef {
    slug: "discord/not-found",
    status: StatusCode::NOT_FOUND,
    title: "No such server or channel",
    description: "The id names nothing this workspace has connected.",
};

pub(crate) const CROSS_SITE: ProblemDef = ProblemDef {
    slug: "discord/cross-site",
    status: StatusCode::FORBIDDEN,
    title: "Cross-site write refused",
    description: "A cookie-authenticated write must come from this site.",
};

pub(crate) struct RouteState {
    pub ctx: ModuleContext,
}

type St = State<Arc<RouteState>>;

pub(crate) fn router(ctx: ModuleContext) -> axum::Router {
    axum::Router::new()
        .route("/", get(summary))
        .route("/connect", get(connect))
        .route("/callback", get(callback))
        .route("/{guild_id}", delete(disconnect))
        .route("/{guild_id}/channels/{channel_id}", patch(set_channel))
        .with_state(Arc::new(RouteState { ctx }))
}

fn store_problem(error: StoreError) -> Problem {
    match error {
        StoreError::Forbidden => Problem::new(&FORBIDDEN),
        StoreError::Store(_) | StoreError::Random | StoreError::Ingest(_) => Problem::internal(),
    }
}

fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// The caller: an owner or admin, by bearer or by session cookie. A cookie
/// write must be same-origin; a bearer write never needs it.
async fn admin(
    state: &RouteState,
    headers: &HeaderMap,
    write: bool,
) -> Result<livingbrain_tokens::Member, Problem> {
    if write && cratefield_core::bearer_token(headers).is_none() {
        livingbrain_tokens::same_origin(headers)
            .map_err(|refused| Problem::new(&CROSS_SITE).with_detail(refused.detail))?;
    }
    let member = livingbrain_tokens::authenticate(&state.ctx.ports, headers).await?;
    if member.is_admin {
        Ok(member)
    } else {
        Err(Problem::new(&FORBIDDEN))
    }
}

fn app(state: &RouteState) -> Result<DiscordApp, Problem> {
    DiscordApp::from_config(state.ctx.config.as_ref()).ok_or_else(|| Problem::new(&NOT_CONFIGURED))
}

/// The settings page with an outcome. A fixed same-origin path.
fn settings_redirect(outcome: &str, reason: Option<&str>) -> Response {
    let mut target = format!("/settings.html?discord={outcome}");
    if let Some(reason) = reason {
        target.push_str("&reason=");
        target.push_str(reason);
    }
    target.push_str("#discord");
    match HeaderValue::from_str(&target) {
        Ok(location) => (StatusCode::FOUND, [(header::LOCATION, location)]).into_response(),
        Err(_) => Problem::internal().into_response(),
    }
}

fn failed(reason: &str) -> Response {
    settings_redirect("error", Some(reason))
}

/// What one channel shows as: where it is in its import.
fn status_of(channel: &Channel) -> &'static str {
    if !channel.present {
        "removed"
    } else if channel.last_error.is_some() && channel.backfill_state != "done" {
        "unavailable"
    } else if channel.backfill_state == "done" {
        "live"
    } else if channel.backfill_state == "running" {
        "backfilling"
    } else {
        "waiting"
    }
}

fn channel_json(channel: &Channel) -> Value {
    json!({
        "channel_id": channel.channel_id,
        "name": channel.name,
        "kind": kind_name(channel.kind),
        "thread": matches!(channel.kind, 10..=12),
        "nsfw": channel.nsfw,
        "included": channel.included,
        "status": status_of(channel),
        "messages": channel.messages,
        "last_error": channel.last_error,
    })
}

/// A channel type as a word the page can show.
fn kind_name(kind: i64) -> &'static str {
    match kind {
        0 => "text",
        2 => "voice",
        5 => "announcement",
        13 => "stage",
        10..=12 => "thread",
        _ => "other",
    }
}

async fn connection_json(db: &dyn Database, connection: &Connection) -> Result<Value, StoreError> {
    let channels = store::channels_of(db, &connection.guild_id).await?;
    let (waiting_days, filed_days) = store::day_counts(db, &connection.guild_id).await?;
    let messages: i64 = channels.iter().map(|c| c.messages).sum();
    let backfilling = channels
        .iter()
        .filter(|c| c.included && c.present && c.backfill_state == "running")
        .min_by(|a, b| a.name.cmp(&b.name))
        .map(|c| c.name.clone());
    let active = connection.disconnected_at.is_none();
    Ok(json!({
        "guild_id": connection.guild_id,
        "guild_name": connection.guild_name,
        "status": if active { "connected" } else { "disconnected" },
        "connected_at": connection.connected_at,
        "listed": connection.channels_synced_at.is_some(),
        "last_error": connection.last_error,
        "progress": {
            "messages_imported": messages,
            "backfilling": backfilling,
            "days_waiting": waiting_days,
            "days_filed": filed_days,
        },
        "channels": channels.iter().map(channel_json).collect::<Vec<_>>(),
    }))
}

/// `GET /v1/discord`: whether the integration is available, and what is
/// connected. Without the Discord secrets, `configured` is `false` and nothing
/// else is listed for the page to show.
async fn summary(State(state): St, headers: HeaderMap) -> Result<Json<Value>, Problem> {
    let member = admin(&state, &headers, false).await?;
    let Some(_app) = DiscordApp::from_config(state.ctx.config.as_ref()) else {
        return Ok(Json(json!({ "configured": false, "connections": [] })));
    };
    let db = port(state.ctx.ports.db.clone())?;
    let mut connections = Vec::new();
    for connection in store::connections_of(&*db, &member.workspace_id)
        .await
        .map_err(store_problem)?
    {
        connections.push(
            connection_json(&*db, &connection)
                .await
                .map_err(store_problem)?,
        );
    }
    Ok(Json(
        json!({ "configured": true, "connections": connections }),
    ))
}

/// `GET /v1/discord/connect`: a redirect to Discord's bot-install page.
async fn connect(State(state): St, headers: HeaderMap) -> Result<Response, Problem> {
    let member = admin(&state, &headers, false).await?;
    let app = app(&state)?;
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let token = state::create(&*db, clock.now(), &member.workspace_id, &member.user_id)
        .await
        .map_err(store_problem)?;
    let redirect = redirect_uri(&state);
    let url = api::authorize_url(&app.client_id, &redirect, &token);
    let location = HeaderValue::from_str(&url).map_err(|_| Problem::internal())?;
    Ok((StatusCode::FOUND, [(header::LOCATION, location)]).into_response())
}

/// The callback URL, as registered on the Discord app.
fn redirect_uri(state: &RouteState) -> String {
    format!(
        "{}/v1/discord/callback",
        livingbrain_pages::app_base(state.ctx.config.as_ref())
    )
}

/// `GET /v1/discord/callback?guild_id&state`: Discord's return after the bot
/// was added.
async fn callback(
    State(state): St,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Ok(member) = livingbrain_tokens::authenticate(&state.ctx.ports, &headers).await else {
        return failed("signed-out");
    };
    if query.contains_key("error") {
        return failed("cancelled");
    }
    let (Some(guild_id), Some(token)) = (query.get("guild_id"), query.get("state")) else {
        return failed("bad-request");
    };
    if !is_snowflake(guild_id) {
        return failed("bad-request");
    }
    let (Ok(db), Ok(clock), Ok(http), Ok(app)) = (
        port(state.ctx.ports.db.clone()),
        port(state.ctx.ports.clock.clone()),
        port(state.ctx.ports.http.clone()),
        app(&state),
    ) else {
        return failed("not-configured");
    };
    let spent = match state::consume(
        &*db,
        clock.now(),
        token,
        &member.workspace_id,
        &member.user_id,
    )
    .await
    {
        Ok(spent) => spent,
        Err(_) => return failed("store"),
    };
    if !spent {
        return failed("state");
    }
    match finish_connect(&*db, &*http, clock.now(), &app, &member, guild_id).await {
        Ok(()) => settings_redirect("connected", None),
        Err(reason) => failed(reason),
    }
}

/// The part of the callback after the state is spent. Returns the reason to
/// show when it does not finish.
async fn finish_connect(
    db: &dyn Database,
    http: &dyn HttpClient,
    now: time::OffsetDateTime,
    app: &DiscordApp,
    member: &livingbrain_tokens::Member,
    guild_id: &str,
) -> Result<(), &'static str> {
    crate::common::require_admin(db, &member.workspace_id, &member.user_id)
        .await
        .map_err(|_| "forbidden")?;
    // The bot must be in the server: Discord confirms it, and names it.
    let reply = api::bot_get(http, &app.bot_token, &format!("/guilds/{guild_id}"))
        .await
        .map_err(|_| "discord")?;
    if reply.status != 200 {
        return Err("not-in-server");
    }
    let guild: Guild = reply.json().ok_or("discord")?;
    if guild.id != guild_id {
        return Err("not-in-server");
    }
    let name: String = guild
        .name
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    match store::connect(
        db,
        now,
        &member.workspace_id,
        guild_id,
        &name,
        &member.user_id,
    )
    .await
    .map_err(|_| "store")?
    {
        ConnectOutcome::Connected => Ok(()),
        ConnectOutcome::BoundElsewhere => Err("bound-elsewhere"),
    }
}

/// `DELETE /v1/discord/{guild_id}`: stops reading the server. Its sources
/// already filed stay. The bot stays in the server until an admin removes it
/// on Discord: a bot cannot leave a server by itself.
async fn disconnect(
    State(state): St,
    headers: HeaderMap,
    Path(guild_id): Path<String>,
) -> Result<Json<Value>, Problem> {
    let member = admin(&state, &headers, true).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    crate::common::require_admin(&*db, &member.workspace_id, &member.user_id)
        .await
        .map_err(store_problem)?;
    if !store::disconnect(&*db, clock.now(), &member.workspace_id, &guild_id)
        .await
        .map_err(store_problem)?
    {
        return Err(Problem::new(&NOT_FOUND));
    }
    Ok(Json(json!({ "disconnected": true })))
}

#[derive(Deserialize)]
struct ChannelPatch {
    included: bool,
}

/// `PATCH /v1/discord/{guild_id}/channels/{channel_id}`: include or exclude.
async fn set_channel(
    State(state): St,
    headers: HeaderMap,
    Path((guild_id, channel_id)): Path<(String, String)>,
    Json(body): Json<ChannelPatch>,
) -> Result<Json<Value>, Problem> {
    let member = admin(&state, &headers, true).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    crate::common::require_admin(&*db, &member.workspace_id, &member.user_id)
        .await
        .map_err(store_problem)?;
    if !store::set_included(
        &*db,
        clock.now(),
        &member.workspace_id,
        &guild_id,
        &channel_id,
        body.included,
    )
    .await
    .map_err(store_problem)?
    {
        return Err(Problem::new(&NOT_FOUND));
    }
    Ok(Json(json!({ "included": body.included })))
}
