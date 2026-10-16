//! The connection and the import, end to end, against a fake Discord. No
//! network: every call goes to [`FakeDiscord`], which logs each call so a test
//! can prove what was (and was not) asked of Discord.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use cratefield_core::axum::body::{Body, Bytes};
use cratefield_core::axum::http::{self, Method, Request, StatusCode, header};
use cratefield_core::{
    Database, HttpClient, HttpError, Kid, MapConfig, Module, Payload, Ports, ScheduledBudget,
    Signer, Statement, UlidIdGen,
};
use cratefield_kms::Kms;
use cratefield_testing::{MemoryBlob, TestHarness};
use livingbrain_pages::Pages;
use livingbrain_workspaces::Workspaces;
use serde_json::{Value, json};
use tower::ServiceExt;

use cratefield_core::ModuleContext;

use crate::DiscordModule;

const W1: &str = "W1SPACE";
const OWNER: &str = "U1OWNER";
const PLAIN: &str = "U1PLAIN";
const GUILD: &str = "900000000000000001";
const GUILD_NAME: &str = "Test Guild";
const GENERAL: &str = "900000000000000011";
const RANDOM: &str = "900000000000000012";
const NSFW: &str = "900000000000000013";
const BASE: &str = "https://app.livingbrain.wiki";
/// 2026-10-03T12:00:00Z: the day in the tests is open, the two before it closed.
const T0: i64 = 1_790_985_600 + 12 * 3600;
const DAY1: i64 = 1_790_812_800;
const DAY2: i64 = DAY1 + 86_400;
const DAY3: i64 = DAY1 + 2 * 86_400;

fn bot_token() -> String {
    "MTIzNDU2Nzg5.fakebot.bot-token-value".to_owned()
}

fn config() -> Vec<(String, String)> {
    [
        ("DISCORD_CLIENT_ID", "1234567890123456789".to_owned()),
        (
            "DISCORD_CLIENT_SECRET",
            "discord-client-secret-value".to_owned(),
        ),
        ("DISCORD_BOT_TOKEN", bot_token()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

fn kms() -> Arc<dyn Kms> {
    use base64::Engine as _;
    let key = base64::engine::general_purpose::STANDARD.encode([9_u8; 32]);
    Arc::new(
        cratefield_kms::WorkerSecretKms::from_lookup(|name| match name {
            "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
            "HARNESS_KEK_V1" => Some(key.clone()),
            _ => None,
        })
        .expect("the key ring is well formed"),
    )
}

/// The snowflake of a message sent at `unix_secs` with sequence `seq`.
fn snow(unix_secs: i64, seq: u64) -> String {
    let ms = u64::try_from(unix_secs * 1000 - 1_420_070_400_000).unwrap();
    ((ms << 22) | seq).to_string()
}

/// The RFC 3339 timestamp Discord sends for `unix_secs`, in UTC.
fn stamp(unix_secs: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(unix_secs).unwrap();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000000+00:00",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn message(unix_secs: i64, seq: u64, text: &str) -> Value {
    json!({
        "id": snow(unix_secs, seq),
        "type": 0,
        "timestamp": stamp(unix_secs),
        "content": text,
        "author": {"id": "2001", "username": "alice", "global_name": "Alice"},
        "attachments": [],
    })
}

fn channel_json(id: &str, name: &str, kind: u8, nsfw: bool) -> Value {
    json!({"id": id, "type": kind, "name": name, "nsfw": nsfw})
}

#[derive(Clone, Default)]
struct Chan {
    messages: Vec<Value>,
    denied: bool,
}

/// A scripted answer: the path needle, the status, the headers.
type Scripted = (String, u16, Vec<(String, String)>);

#[derive(Default)]
struct DiscordState {
    channels: Vec<Value>,
    chans: HashMap<String, Chan>,
    /// Scripted answers: the next call whose path contains the needle gets
    /// this status and these headers instead.
    scripted: VecDeque<Scripted>,
    /// `GET path` of every call, in order.
    log: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeDiscord(Arc<Mutex<DiscordState>>);

impl FakeDiscord {
    fn with_guild() -> Self {
        let fake = Self::default();
        {
            let mut state = fake.0.lock().unwrap();
            state.channels = vec![
                channel_json(GENERAL, "general", 0, false),
                channel_json(RANDOM, "random", 0, false),
                channel_json(NSFW, "adults", 0, true),
            ];
            state.chans.insert(GENERAL.to_owned(), Chan::default());
            state.chans.insert(RANDOM.to_owned(), Chan::default());
            state.chans.insert(NSFW.to_owned(), Chan::default());
        }
        fake
    }

    fn post(&self, channel: &str, message: Value) {
        self.0
            .lock()
            .unwrap()
            .chans
            .get_mut(channel)
            .unwrap()
            .messages
            .push(message);
    }

    fn script(&self, needle: &str, status: u16, headers: &[(&str, &str)]) {
        self.0.lock().unwrap().scripted.push_back((
            needle.to_owned(),
            status,
            headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        ));
    }

    fn clear_log(&self) {
        self.0.lock().unwrap().log.clear();
    }

    fn log(&self) -> Vec<String> {
        self.0.lock().unwrap().log.clone()
    }

    fn route(&self, path: &str) -> (u16, Value) {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let params: HashMap<&str, &str> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .collect();
        let state = self.0.lock().unwrap();
        if path == format!("/guilds/{GUILD}") {
            return (200, json!({"id": GUILD, "name": GUILD_NAME}));
        }
        if path == format!("/guilds/{GUILD}/channels") {
            return (200, Value::Array(state.channels.clone()));
        }
        if path == format!("/guilds/{GUILD}/threads/active") {
            return (200, json!({"threads": [], "members": []}));
        }
        if let Some(rest) = path.strip_prefix("/channels/") {
            let (channel, action) = rest.split_once('/').unwrap_or((rest, ""));
            if action == "threads/archived/public" {
                return (200, json!({"threads": [], "has_more": false}));
            }
            let Some(chan) = state.chans.get(channel).cloned() else {
                return (404, json!({"message": "Unknown Channel"}));
            };
            if chan.denied {
                return (403, json!({"message": "Missing Access"}));
            }
            if action == "messages" {
                let limit: usize = params
                    .get("limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(50);
                let mut all = chan.messages.clone();
                all.sort_by_key(|m| m["id"].as_str().unwrap().parse::<u64>().unwrap());
                let num = |m: &Value| m["id"].as_str().unwrap().parse::<u64>().unwrap();
                let page: Vec<Value> = if let Some(before) = params.get("before") {
                    let before: u64 = before.parse().unwrap();
                    all.into_iter()
                        .filter(|m| num(m) < before)
                        .rev()
                        .take(limit)
                        .collect()
                } else if let Some(after) = params.get("after") {
                    let after: u64 = after.parse().unwrap();
                    all.into_iter()
                        .filter(|m| num(m) > after)
                        .take(limit)
                        .collect()
                } else {
                    all.into_iter().rev().take(limit).collect()
                };
                return (200, Value::Array(page));
            }
        }
        (404, json!({"message": "404: Not Found"}))
    }
}

#[async_trait::async_trait]
impl HttpClient for FakeDiscord {
    async fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let uri = request.uri().to_string();
        let Some(path) = uri.strip_prefix("https://discord.com/api/v10") else {
            return Err(HttpError::Transport("not Discord".to_owned()));
        };
        assert!(
            request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == format!("Bot {}", bot_token())),
            "every Discord call is made with the bot token"
        );
        self.0.lock().unwrap().log.push(path.to_owned());
        let scripted = {
            let mut state = self.0.lock().unwrap();
            state
                .scripted
                .iter()
                .position(|(needle, _, _)| path.contains(needle.as_str()))
                .and_then(|index| state.scripted.remove(index))
        };
        if let Some((_, status, headers)) = scripted {
            let mut response = http::Response::builder().status(status);
            for (k, v) in headers {
                response = response.header(k, v);
            }
            return Ok(response
                .body(Bytes::from_static(b"{\"message\":\"scripted\"}"))
                .unwrap());
        }
        let (status, body) = self.route(path);
        Ok(http::Response::builder()
            .status(status)
            .header("x-ratelimit-remaining", "4")
            .body(Bytes::from(body.to_string()))
            .unwrap())
    }
}

struct Kit {
    kit: TestHarness,
    config: Vec<(String, String)>,
    discord: FakeDiscord,
    blob: Arc<MemoryBlob>,
}

struct Res {
    status: StatusCode,
    location: Option<String>,
    body: Vec<u8>,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

fn kit_with(config: Vec<(String, String)>, discord: FakeDiscord) -> Kit {
    let blob = Arc::new(MemoryBlob::new());
    let (http_client, store) = (discord.clone(), blob.clone());
    let mut config_all = vec![("WORKSPACES_REDIRECT_BASE".to_owned(), BASE.to_owned())];
    config_all.extend(config.clone());
    let ports_config = config_all.clone();
    let kit = TestHarness::with_ports(
        vec![
            Box::new(Workspaces::new()),
            Box::new(Pages::new()),
            Box::new(DiscordModule::new().with_kms(kms())),
        ],
        move |ports| {
            ports.http = Some(Arc::new(http_client));
            ports.blob = Some(store);
            ports.config = Arc::new(MapConfig::from_pairs(ports_config));
        },
    );
    Kit {
        kit,
        config: config_all,
        discord,
        blob,
    }
}

fn kit() -> Kit {
    let discord = FakeDiscord::with_guild();
    kit_with(config(), discord)
}

impl Kit {
    fn db(&self) -> &dyn Database {
        &*self.kit.db
    }

    fn cookie(&self, user: &str) -> String {
        self.kit.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({"team_id": W1, "user_id": user, "exp": 2_000_000_000}).to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    }

    async fn seed(&self) {
        self.db()
            .execute(&Statement::with_values(
                "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
                vec![
                    crate::common::text(W1),
                    crate::common::text("World-360"),
                    crate::common::text(OWNER),
                    crate::common::text("2026-01-01T00:00:00Z"),
                ],
            ))
            .await
            .unwrap();
        for (user, role) in [(OWNER, "owner"), (PLAIN, "member")] {
            self.db()
                .execute(&Statement::with_values(
                    "INSERT INTO workspace_members (workspace_id, user_id, name, is_admin, updated_at, role) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                    vec![
                        crate::common::text(W1),
                        crate::common::text(user),
                        crate::common::text("someone"),
                        crate::common::int(i64::from(role != "member")),
                        crate::common::text("2026-01-01T00:00:00Z"),
                        crate::common::text(role),
                    ],
                ))
                .await
                .unwrap();
        }
    }

    async fn send(&self, method: Method, path: &str, cookie: Option<&str>, body: Vec<u8>) -> Res {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(cookie) = cookie {
            builder = builder.header(
                header::COOKIE,
                format!("{}={cookie}", livingbrain_workspaces::SESSION_COOKIE),
            );
        }
        let request = builder
            .header(header::CONTENT_LENGTH, body.len().to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = self.kit.router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = cratefield_core::axum::body::to_bytes(body, 4 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        let location = parts
            .headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        Res {
            status: parts.status,
            location,
            body,
        }
    }

    async fn get(&self, path: &str, cookie: &str) -> Res {
        self.send(Method::GET, path, Some(cookie), Vec::new()).await
    }

    async fn write(&self, method: Method, path: &str, cookie: &str, body: Value) -> Res {
        self.send(method, path, Some(cookie), body.to_string().into_bytes())
            .await
    }

    /// The module's context at `at`, as the harness gives it (its blob is the
    /// module's own key space, as in production).
    fn context(&self, at: i64) -> (Arc<dyn Module>, ModuleContext) {
        let module: Arc<dyn Module> = self
            .kit
            .modules
            .iter()
            .find(|m| m.name() == "discord")
            .cloned()
            .unwrap();
        let mut ports = Ports::with_config(Arc::new(MapConfig::from_pairs(self.config.clone())));
        ports.db = Some(self.kit.db.clone());
        ports.clock = Some(Arc::new(cratefield_testing::FixedClock(
            time::OffsetDateTime::from_unix_timestamp(at).unwrap(),
        )));
        ports.http = Some(Arc::new(self.discord.clone()));
        ports.blob = Some(self.blob.clone());
        ports.id_gen = Some(Arc::new(UlidIdGen));
        ports.defer = Some(Arc::new(self.kit.defer.clone()));
        ports.signer = Some(self.kit.signer.clone());
        let ctx = self.kit.harness.module_context(module.as_ref(), &ports);
        (module, ctx)
    }

    /// Runs the module's cron entry point at `at` with a budget of `steps` calls.
    async fn tick(&self, at: i64, steps: u32) {
        let (module, mut ctx) = self.context(at);
        ctx.scheduled = Arc::new(ScheduledBudget::new(None, Some(steps)));
        module
            .scheduled(&ctx, "*/5 * * * *")
            .await
            .expect("the drain runs");
    }

    /// The source rows the import filed: `(rel_path, body)` for each, read back
    /// through the page store.
    async fn filed(&self) -> Vec<(String, String)> {
        let scope = livingbrain_pages::page_scope(W1, &livingbrain_access::Scope::Shared);
        let (_, ctx) = self.context(T0);
        let blob = ctx.ports.blob.clone().expect("the module has a blob");
        let rows = self
            .db()
            .query(&Statement::with_values(
                "SELECT rel_path, body_sha256 FROM sources WHERE kind = ? AND scope = ? ORDER BY rel_path, id",
                vec![crate::common::text("discord"), crate::common::text(&scope)],
            ))
            .await
            .unwrap();
        let store = livingbrain_pages::SourceStore::new(
            self.kit.db.clone(),
            blob,
            kms(),
            Arc::new(cratefield_testing::FixedClock(
                time::OffsetDateTime::from_unix_timestamp(T0).unwrap(),
            )),
            Arc::new(UlidIdGen),
        );
        let mut out = Vec::new();
        for row in &rows.rows {
            let rel: String = row.get("rel_path").unwrap();
            let sha: String = row.get("body_sha256").unwrap();
            out.push((rel, store.open(&scope, &sha).await.unwrap()));
        }
        out
    }
}

/// Connects the guild as the owner, through the real routes. Returns the
/// callback's location.
async fn connect_as_owner(kit: &Kit) -> String {
    let cookie = kit.cookie(OWNER);
    let started = kit.get("/v1/discord/connect", &cookie).await;
    assert_eq!(
        started.status,
        StatusCode::FOUND,
        "{}",
        String::from_utf8_lossy(&started.body)
    );
    let location = started.location.expect("a redirect");
    assert!(
        location.starts_with("https://discord.com/oauth2/authorize?"),
        "{location}"
    );
    assert!(location.contains("&scope=bot&"), "{location}");
    assert!(location.contains("&permissions=66560&"), "{location}");
    let state = location.rsplit("&state=").next().unwrap().to_owned();
    let callback = kit
        .get(
            &format!("/v1/discord/callback?guild_id={GUILD}&code=unused&state={state}"),
            &cookie,
        )
        .await;
    assert_eq!(callback.status, StatusCode::FOUND);
    callback.location.unwrap()
}

async fn the_connect_state_is_verified_spent_and_bound_to_its_minter_body() {
    let kit = kit();
    kit.seed().await;
    let owner = kit.cookie(OWNER);
    let started = kit.get("/v1/discord/connect", &owner).await;
    let state = started
        .location
        .unwrap()
        .rsplit("&state=")
        .next()
        .unwrap()
        .to_owned();

    // A forged state is refused and records nothing.
    let forged = kit
        .get(
            &format!("/v1/discord/callback?guild_id={GUILD}&state=forged-value"),
            &owner,
        )
        .await;
    assert!(forged.location.unwrap().contains("reason=state"));
    assert!(
        crate::store::connection(kit.db(), GUILD)
            .await
            .unwrap()
            .is_none()
    );

    // Another member cannot spend the owner's state.
    let other = kit
        .get(
            &format!("/v1/discord/callback?guild_id={GUILD}&state={state}"),
            &kit.cookie(PLAIN),
        )
        .await;
    assert!(other.location.unwrap().contains("reason=state"));

    // An expired state is refused.
    kit.db()
        .execute(&Statement::with_values(
            "UPDATE discord_states SET expires_at = 0",
            vec![],
        ))
        .await
        .unwrap();
    let expired = kit
        .get(
            &format!("/v1/discord/callback?guild_id={GUILD}&state={state}"),
            &owner,
        )
        .await;
    assert!(expired.location.unwrap().contains("reason=state"));

    // A fresh state, spent by its minter, connects the server and names it.
    let connected = connect_as_owner(&kit).await;
    assert!(connected.contains("discord=connected"), "{connected}");
    let row = crate::store::connection(kit.db(), GUILD)
        .await
        .unwrap()
        .expect("connected");
    assert_eq!(row.guild_name, GUILD_NAME);
    assert_eq!(row.workspace_id, W1);

    // A replay of the spent state is refused.
    let replay = kit
        .get(
            &format!("/v1/discord/callback?guild_id={GUILD}&state={state}"),
            &owner,
        )
        .await;
    assert!(replay.location.unwrap().contains("reason=state"));
}

#[test]
fn the_connect_state_is_verified_spent_and_bound_to_its_minter() {
    pollster::block_on(the_connect_state_is_verified_spent_and_bound_to_its_minter_body());
}

async fn a_plain_member_is_refused_every_discord_route_body() {
    let kit = kit();
    kit.seed().await;
    let plain = kit.cookie(PLAIN);
    assert_eq!(
        kit.get("/v1/discord", &plain).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        kit.get("/v1/discord/connect", &plain).await.status,
        StatusCode::FORBIDDEN
    );
    let patched = kit
        .write(
            Method::PATCH,
            &format!("/v1/discord/{GUILD}/channels/{GENERAL}"),
            &plain,
            json!({"included": false}),
        )
        .await;
    assert_eq!(patched.status, StatusCode::FORBIDDEN);
    assert_eq!(
        kit.write(
            Method::DELETE,
            &format!("/v1/discord/{GUILD}"),
            &plain,
            json!({})
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

#[test]
fn a_plain_member_is_refused_every_discord_route() {
    pollster::block_on(a_plain_member_is_refused_every_discord_route_body());
}

async fn a_forged_callback_cannot_connect_a_server_for_a_plain_member_body() {
    let kit = kit();
    kit.seed().await;
    let owner = kit.cookie(OWNER);
    let started = kit.get("/v1/discord/connect", &owner).await;
    let state = started
        .location
        .unwrap()
        .rsplit("&state=")
        .next()
        .unwrap()
        .to_owned();
    // The plain member asks the callback with the owner's state: refused, and
    // the owner's state is not spent by it.
    let refused = kit
        .get(
            &format!("/v1/discord/callback?guild_id={GUILD}&state={state}"),
            &kit.cookie(PLAIN),
        )
        .await;
    assert!(refused.location.unwrap().contains("reason=state"));
    assert!(
        crate::store::connection(kit.db(), GUILD)
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_forged_callback_cannot_connect_a_server_for_a_plain_member() {
    pollster::block_on(a_forged_callback_cannot_connect_a_server_for_a_plain_member_body());
}

async fn backfill_pages_backwards_until_empty_then_files_each_day_body() {
    let kit = kit();
    kit.seed().await;
    // Two closed days and today, with a system message that must not be kept.
    for seq in 1..=3 {
        kit.discord.post(
            GENERAL,
            message(DAY1 + 60 * seq, seq as u64, &format!("day one {seq}")),
        );
    }
    for seq in 1..=2 {
        kit.discord.post(
            GENERAL,
            message(DAY2 + 60 * seq, 10 + seq as u64, &format!("day two {seq}")),
        );
    }
    let mut system = message(DAY3 + 60, 20, "joined");
    system["type"] = json!(7);
    kit.discord.post(GENERAL, system);
    kit.discord
        .post(GENERAL, message(DAY3 + 120, 21, "today one"));
    kit.discord
        .post(RANDOM, message(DAY1 + 90, 1, "random hello"));
    connect_as_owner(&kit).await;

    kit.discord.clear_log();
    kit.tick(T0, 200).await;

    // History is read newest page first, `before` each page's oldest id, until
    // an empty page.
    let general: Vec<String> = kit
        .discord
        .log()
        .into_iter()
        .filter(|p| p.starts_with(&format!("/channels/{GENERAL}/messages")))
        .collect();
    assert!(
        general[0].contains("limit=100") && !general[0].contains("before="),
        "{general:?}"
    );
    assert!(general[1].contains("before="), "{general:?}");
    let channel = crate::store::channel(kit.db(), GUILD, GENERAL)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(channel.backfill_state, "done");
    assert_eq!(channel.messages, 6, "the system message is not counted");
    assert!(channel.polled_at.is_some());

    let filed = kit.filed().await;
    let days: Vec<&str> = filed.iter().map(|(rel, _)| rel.as_str()).collect();
    assert!(
        days.contains(&format!("discord/{GUILD}/{GENERAL}/2026-10-01").as_str()),
        "{days:?}"
    );
    assert!(
        days.contains(&format!("discord/{GUILD}/{GENERAL}/2026-10-03").as_str()),
        "{days:?}"
    );
    let today = filed
        .iter()
        .find(|(rel, _)| rel.ends_with("2026-10-03") && rel.contains(GENERAL))
        .unwrap();
    assert!(today.1.contains("today one"), "{}", today.1);
    assert!(!today.1.contains("joined"), "{}", today.1);
    assert!(today.1.contains("# #general in Test Guild"), "{}", today.1);
    // The nsfw channel was excluded by default: nothing was read from it.
    assert!(kit.discord.log().iter().all(|p| !p.contains(NSFW)));
}

#[test]
fn backfill_pages_backwards_until_empty_then_files_each_day() {
    pollster::block_on(backfill_pages_backwards_until_empty_then_files_each_day_body());
}

async fn new_messages_are_polled_forward_and_a_busy_day_is_refiled_only_after_the_hour_body() {
    let kit = kit();
    kit.seed().await;
    kit.discord
        .post(GENERAL, message(DAY3 + 60, 1, "first today"));
    connect_as_owner(&kit).await;
    kit.tick(T0, 200).await;
    let before = kit.filed().await.len();

    // A message half an hour later: polled forward (after=), the day is dirty
    // but was filed less than an hour ago, so nothing new is filed yet.
    kit.discord
        .post(GENERAL, message(DAY3 + 3600, 2, "second today"));
    kit.discord.clear_log();
    kit.tick(T0 + 1800, 200).await;
    assert!(
        kit.discord.log().iter().any(|p| p.contains("after=")),
        "{:?}",
        kit.discord.log()
    );
    assert_eq!(kit.filed().await.len(), before, "throttled within the hour");

    // Past the hour, the day is re-read whole and filed as a new version.
    kit.tick(T0 + 2 * 3600 + 60, 200).await;
    let versions: Vec<String> = kit
        .filed()
        .await
        .into_iter()
        .filter(|(rel, _)| rel.ends_with("2026-10-03"))
        .map(|(_, body)| body)
        .collect();
    assert!(versions.len() >= 2, "{versions:?}");
    assert!(
        versions
            .iter()
            .any(|body| body.contains("first today") && body.contains("second today"))
    );
}

#[test]
fn new_messages_are_polled_forward_and_a_busy_day_is_refiled_only_after_the_hour() {
    pollster::block_on(
        new_messages_are_polled_forward_and_a_busy_day_is_refiled_only_after_the_hour_body(),
    );
}

async fn a_rate_limit_stops_the_tick_and_the_next_tick_resumes_body() {
    let kit = kit();
    kit.seed().await;
    kit.discord.post(GENERAL, message(DAY1 + 60, 1, "general"));
    kit.discord.post(RANDOM, message(DAY1 + 60, 2, "random"));
    connect_as_owner(&kit).await;
    // The first read of #random is rate limited for two seconds.
    kit.discord.script(
        &format!("/channels/{RANDOM}/messages"),
        429,
        &[("retry-after", "2"), ("x-ratelimit-global", "false")],
    );
    kit.discord.clear_log();
    kit.tick(T0, 200).await;
    let log = kit.discord.log();
    let limited = log
        .iter()
        .position(|p| p.contains(&format!("/channels/{RANDOM}/messages")))
        .expect("asked");
    assert_eq!(
        limited,
        log.len() - 1,
        "nothing is asked after the 429: {log:?}"
    );
    let random = crate::store::channel(kit.db(), GUILD, RANDOM)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        random.backfill_state, "pending",
        "its backfill has not started"
    );

    // The next tick, after the wait, reads #random.
    kit.discord.clear_log();
    kit.tick(T0 + 60, 200).await;
    let random = crate::store::channel(kit.db(), GUILD, RANDOM)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(random.backfill_state, "done");
    assert!(
        kit.filed()
            .await
            .iter()
            .any(|(rel, body)| rel.contains(RANDOM) && body.contains("random"))
    );
}

#[test]
fn a_rate_limit_stops_the_tick_and_the_next_tick_resumes() {
    pollster::block_on(a_rate_limit_stops_the_tick_and_the_next_tick_resumes_body());
}

async fn an_excluded_channel_is_never_read_and_including_it_resumes_body() {
    let kit = kit();
    kit.seed().await;
    kit.discord.post(RANDOM, message(DAY1 + 60, 1, "random"));
    connect_as_owner(&kit).await;
    // The first tick lists the server's channels and imports them.
    kit.tick(T0, 200).await;
    let cookie = kit.cookie(OWNER);
    let excluded = kit
        .write(
            Method::PATCH,
            &format!("/v1/discord/{GUILD}/channels/{RANDOM}"),
            &cookie,
            json!({"included": false}),
        )
        .await;
    assert_eq!(excluded.status, StatusCode::OK);
    // A new message, and a listing an hour on: still nothing is read from it.
    kit.discord
        .post(RANDOM, message(DAY3 + 60, 2, "while excluded"));
    kit.discord.clear_log();
    kit.tick(T0 + 3700, 200).await;
    assert!(
        kit.discord
            .log()
            .iter()
            .all(|p| !p.contains(&format!("/channels/{RANDOM}/"))),
        "{:?}",
        kit.discord.log()
    );
    assert!(
        kit.filed()
            .await
            .iter()
            .all(|(_, body)| !body.contains("while excluded"))
    );

    let included = kit
        .write(
            Method::PATCH,
            &format!("/v1/discord/{GUILD}/channels/{RANDOM}"),
            &cookie,
            json!({"included": true}),
        )
        .await;
    assert_eq!(included.status, StatusCode::OK);
    kit.tick(T0 + 3800, 200).await;
    assert!(
        kit.filed()
            .await
            .iter()
            .any(|(_, body)| body.contains("while excluded"))
    );
}

#[test]
fn an_excluded_channel_is_never_read_and_including_it_resumes() {
    pollster::block_on(an_excluded_channel_is_never_read_and_including_it_resumes_body());
}

async fn disconnect_stops_every_read_of_the_server_body() {
    let kit = kit();
    kit.seed().await;
    kit.discord.post(GENERAL, message(DAY1 + 60, 1, "before"));
    connect_as_owner(&kit).await;
    kit.tick(T0, 200).await;
    let cookie = kit.cookie(OWNER);
    let gone = kit
        .write(
            Method::DELETE,
            &format!("/v1/discord/{GUILD}"),
            &cookie,
            json!({}),
        )
        .await;
    assert_eq!(gone.status, StatusCode::OK);
    kit.discord
        .post(GENERAL, message(DAY3 + 60, 2, "after disconnect"));
    kit.discord.clear_log();
    kit.tick(T0 + 3600, 200).await;
    assert!(
        kit.discord.log().iter().all(|p| !p.contains(GUILD)),
        "{:?}",
        kit.discord.log()
    );
    let summary = kit.get("/v1/discord", &cookie).await.json();
    assert_eq!(summary["connections"][0]["status"], "disconnected");
}

#[test]
fn disconnect_stops_every_read_of_the_server() {
    pollster::block_on(disconnect_stops_every_read_of_the_server_body());
}

async fn without_real_secrets_the_integration_is_unavailable_body() {
    let placeholder: Vec<(String, String)> = config()
        .into_iter()
        .map(|(k, v)| {
            if k == "DISCORD_BOT_TOKEN" {
                (k, "replace-with-your-bot-token".to_owned())
            } else {
                (k, v)
            }
        })
        .collect();
    let kit = kit_with(placeholder, FakeDiscord::with_guild());
    kit.seed().await;
    let cookie = kit.cookie(OWNER);
    let summary = kit.get("/v1/discord", &cookie).await.json();
    assert_eq!(summary, json!({"configured": false, "connections": []}));
    assert_eq!(
        kit.get("/v1/discord/connect", &cookie).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn without_real_secrets_the_integration_is_unavailable() {
    pollster::block_on(without_real_secrets_the_integration_is_unavailable_body());
}

async fn the_status_names_each_channels_progress_and_the_running_backfill_body() {
    let kit = kit();
    kit.seed().await;
    kit.discord.post(GENERAL, message(DAY1 + 60, 1, "one"));
    kit.discord.post(GENERAL, message(DAY1 + 70, 2, "two"));
    connect_as_owner(&kit).await;
    kit.tick(T0, 200).await;
    let summary = kit.get("/v1/discord", &kit.cookie(OWNER)).await.json();
    let connection = &summary["connections"][0];
    assert_eq!(connection["guild_name"], GUILD_NAME);
    assert_eq!(connection["progress"]["messages_imported"], 2);
    let general = connection["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channel_id"] == GENERAL)
        .unwrap();
    assert_eq!(general["status"], "live");
    assert_eq!(general["included"], true);
    let nsfw = connection["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channel_id"] == NSFW)
        .unwrap();
    assert_eq!(nsfw["included"], false);
}

#[test]
fn the_status_names_each_channels_progress_and_the_running_backfill() {
    pollster::block_on(the_status_names_each_channels_progress_and_the_running_backfill_body());
}

#[test]
fn only_the_scheduled_drain_mints_a_drain() {
    let files = [
        ("lib.rs", include_str!("lib.rs")),
        ("routes.rs", include_str!("routes.rs")),
        ("state.rs", include_str!("state.rs")),
        ("store.rs", include_str!("store.rs")),
        ("sync.rs", include_str!("sync.rs")),
        ("api.rs", include_str!("api.rs")),
        ("render.rs", include_str!("render.rs")),
        ("config.rs", include_str!("config.rs")),
    ];
    for (name, source) in files {
        assert!(
            !source.contains("for_scheduled_run"),
            "{name} must not mint a Drain"
        );
    }
}
