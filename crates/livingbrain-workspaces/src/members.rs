//! Members, roles, invitations and the workspace switcher (ADR 0004, phase a).
//!
//! Every route here is a **session** route: the workspace and the acting
//! member come from the verified cookie through [`session_scope`], never
//! from the request, and a personal access token cannot reach them — a
//! credential that could add people to a workspace is not one a script
//! should hold. Every write takes the same-origin check the module's other
//! cookie writes take.
//!
//! ## Who may do what
//!
//! | Action | Who |
//! |---|---|
//! | list members | any member |
//! | invite, change a role, remove a member | an owner or admin |
//! | anything that makes, unmakes or removes an **owner** | an owner |
//! | remove yourself (leave) | anyone, unless you are the last owner |
//!
//! A workspace always keeps one owner: demoting or removing the last one is
//! a `409`, decided by one guarded statement ([`store::set_role`],
//! [`store::remove_member`]).
//!
//! ## Removal
//!
//! The member row is tombstoned and their sign-in identities deleted, which
//! ends every session and personal access token they hold on the next
//! request — both re-read the member row every time, and a removed row is no
//! row. A Slack sign-in by a removed person lands on the tombstone and is
//! refused. The composition's [`MemberRemoval`](crate::MemberRemoval) seam
//! then cleans up what this module cannot reach (token rows, agent accounts,
//! grants).
//!
//! ## Invitations
//!
//! An invitation mails a sign-in link for the workspace with the
//! invitation's lifetime (seven days). Spending it is the ordinary
//! `POST /email/verify`: an address that is not yet a member but holds an
//! open invitation becomes a member with the invited role. While the
//! invitation is open, a fresh magic link for the same workspace and address
//! accepts it too, so a lost mail is not a lost invitation.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::{Problem, ProblemDef, SendOutcome, invalid_email_problem, normalize_email};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;

use crate::flow::{self, Session};
use crate::handlers::{
    ModuleState, database, mail_failed, new_user_id, new_workspace_id, port, require_same_origin,
    session_scope, sign_in_token, token_hash,
};
use crate::profile::{self, AvatarRef};
use crate::store::{self, Member, Role, RosterOutcome};
use crate::{Invitation, SignInMailInput};

/// How long an invitation, and the link it mails, stays usable.
pub(crate) const INVITE_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// An action the caller's role does not allow.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "workspaces/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Your role does not allow this",
    description: "Ask an owner or admin of the workspace.",
};

/// A member or invitation id that is not in this workspace.
const NOT_FOUND: ProblemDef = ProblemDef {
    slug: "workspaces/not-found",
    status: StatusCode::NOT_FOUND,
    title: "Not found",
    description: "There is no such member or invitation in this workspace.",
};

/// The change would leave the workspace without an owner.
const LAST_OWNER: ProblemDef = ProblemDef {
    slug: "workspaces/last-owner",
    status: StatusCode::CONFLICT,
    title: "A workspace keeps at least one owner",
    description: "Make somebody else an owner first.",
};

/// The address already belongs to a member.
const ALREADY_A_MEMBER: ProblemDef = ProblemDef {
    slug: "workspaces/already-a-member",
    status: StatusCode::CONFLICT,
    title: "That address is already a member",
    description: "Change their role instead.",
};

/// A role this deployment does not accept.
const UNKNOWN_ROLE: ProblemDef = ProblemDef {
    slug: "workspaces/unknown-role",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Unknown role",
    description: "A role is owner, admin or member. The read-only viewer role arrives \
                  with access grants (ADR 0004, phase c).",
};

/// The typed name is not the workspace's name.
const CONFIRM_MISMATCH: ProblemDef = ProblemDef {
    slug: "workspaces/confirm-mismatch",
    status: StatusCode::BAD_REQUEST,
    title: "The name does not match",
    description: "Type the workspace name exactly as it is shown.",
};

/// The person has no other workspace to go to.
const ONLY_WORKSPACE: ProblemDef = ProblemDef {
    slug: "workspaces/only-workspace",
    status: StatusCode::CONFLICT,
    title: "This is your only workspace.",
    description: "Join or create another workspace first, then delete this one.",
};

/// The mailer is not wired.
const MAIL_NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "workspaces/mail-not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "Invitations need email",
    description: "An operator wires a Mailer port and sets WORKSPACES_MAIL_FROM.",
};

fn forbidden() -> Problem {
    Problem::new(&FORBIDDEN)
}

/// The roles a request may name. `viewer` is refused until access grants
/// can make it mean read-only everywhere: a role that says "read-only" and
/// is not would be worse than no such role.
fn requested_role(value: &str) -> Result<Role, Problem> {
    match Role::parse(value.trim()) {
        Some(role @ (Role::Owner | Role::Admin | Role::Member)) => Ok(role),
        _ => Err(Problem::new(&UNKNOWN_ROLE)),
    }
}

fn now_text(state: &ModuleState) -> Result<(time::OffsetDateTime, String), Problem> {
    let clock = port(state.ctx.ports.clock.clone())?;
    let now = clock.now();
    Ok((now, now.format(&Rfc3339).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// Members

#[derive(Debug, Deserialize)]
pub(crate) struct RoleBody {
    role: String,
}

/// `PATCH /members/{user_id}` — change a member's role.
pub(crate) async fn change_role(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<RoleBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    let role = requested_role(&body.role)?;
    let db = port(state.ctx.ports.db.clone())?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let Some(target) = store::member(&*db, &workspace.id, &user_id)
        .await
        .map_err(database)?
    else {
        return Err(Problem::new(&NOT_FOUND));
    };
    // Making, unmaking or touching an owner is an owner's call.
    if (target.role == Role::Owner || role == Role::Owner) && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let (_, now) = now_text(&state)?;
    match store::set_role(&*db, &workspace.id, &user_id, role, &now)
        .await
        .map_err(database)?
    {
        RosterOutcome::Done => {}
        RosterOutcome::NotFound => return Err(Problem::new(&NOT_FOUND)),
        RosterOutcome::LastOwner => return Err(Problem::new(&LAST_OWNER)),
    }
    let updated = store::member(&*db, &workspace.id, &user_id)
        .await
        .map_err(database)?
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    Ok(Json(MemberView::from(updated)).into_response())
}

/// `DELETE /members/{user_id}` — remove a member, or leave.
pub(crate) async fn remove(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let leaving = actor.user_id == user_id;
    if !leaving && !actor.may_administer() {
        return Err(forbidden());
    }
    let Some(target) = store::member(&*db, &workspace.id, &user_id)
        .await
        .map_err(database)?
    else {
        return Err(Problem::new(&NOT_FOUND));
    };
    if !leaving && target.role == Role::Owner && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let (now, now_text) = now_text(&state)?;
    match store::remove_member(&*db, &workspace.id, &user_id, &now_text)
        .await
        .map_err(database)?
    {
        RosterOutcome::Done => {}
        RosterOutcome::NotFound => return Err(Problem::new(&NOT_FOUND)),
        RosterOutcome::LastOwner => return Err(Problem::new(&LAST_OWNER)),
    }
    if let Some(hook) = &state.removal {
        hook.removed(&*db, now, &workspace.id, &user_id).await;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    // Leaving ends this browser's session too, not only every later one.
    if leaving {
        response.headers_mut().append(
            cratefield_core::axum::http::header::SET_COOKIE,
            flow::clear_cookie(flow::SESSION_COOKIE).ok_or_else(Problem::internal)?,
        );
    }
    Ok(response)
}

/// One member, as the roster lists them.
#[derive(Debug, Serialize)]
pub(crate) struct MemberView {
    pub user_id: String,
    /// The name everybody reads: the chosen one, else the reported one.
    pub name: String,
    /// The name the member chose, or `null`.
    pub display_name: Option<String>,
    /// The same-origin URL of the member's avatar, or `null`.
    pub avatar_url: Option<String>,
    pub timezone: Option<String>,
    /// Slack's admin flag.
    pub is_admin: bool,
    /// `owner`, `admin`, `member` or `viewer`.
    pub role: &'static str,
}

impl From<Member> for MemberView {
    fn from(member: Member) -> Self {
        Self {
            avatar_url: AvatarRef::of(member.avatar.as_deref()).map(|avatar| avatar.url),
            display_name: member.display_name,
            user_id: member.user_id,
            name: member.name,
            timezone: member.timezone,
            is_admin: member.is_admin,
            role: member.role.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Invitations

#[derive(Debug, Deserialize)]
pub(crate) struct InviteBody {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Serialize)]
struct InviteView {
    id: String,
    email: String,
    role: &'static str,
    invited_by: String,
    created_at: String,
    expires_at: String,
}

impl From<store::Invite> for InviteView {
    fn from(invite: store::Invite) -> Self {
        Self {
            id: invite.id,
            email: invite.email,
            role: invite.role.as_str(),
            invited_by: invite.invited_by,
            created_at: invite.created_at,
            expires_at: invite.expires_at,
        }
    }
}

/// `POST /invites` — invite an address with a role, and mail the link.
pub(crate) async fn invite(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<InviteBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let role = requested_role(body.role.as_deref().unwrap_or("member"))?;
    if role == Role::Owner && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let email = normalize_email(&body.email);
    if let Some(problem) = cratefield_core::validation_error(&email).map(invalid_email_problem) {
        return Err(problem);
    }
    let db = port(state.ctx.ports.db.clone())?;
    if store::member_for_identity(&*db, &workspace.id, "email", &email)
        .await
        .map_err(database)?
        .is_some()
    {
        return Err(Problem::new(&ALREADY_A_MEMBER));
    }
    let mailer = state
        .ctx
        .ports
        .mailer
        .clone()
        .ok_or_else(|| Problem::new(&MAIL_NOT_CONFIGURED))?;
    let public_base = state.public_base()?.to_owned();
    let id_gen = port(state.ctx.ports.id_gen.clone())?;
    let (now, now_text) = now_text(&state)?;
    let expires_at = (now + time::Duration::seconds(INVITE_TTL_SECS))
        .format(&Rfc3339)
        .unwrap_or_default();

    let token = sign_in_token(&*id_gen);
    let link_hash = token_hash(&token);
    store::put_sign_in_link(
        &*db,
        &link_hash,
        &email,
        Some(&workspace.id),
        "",
        &expires_at,
        &now_text,
    )
    .await
    .map_err(database)?;
    let id = format!("inv_{}", id_gen.ulid());
    store::put_invite(
        &*db,
        &store::NewInvite {
            id: &id,
            workspace_id: &workspace.id,
            email: &email,
            role,
            invited_by: &actor.user_id,
            link_hash: &link_hash,
            expires_at: &expires_at,
        },
        &now_text,
    )
    .await
    .map_err(database)?;

    let link = format!("{public_base}/v1/workspaces/email/verify?token={token}");
    let message = state.message(
        &email,
        &link,
        SignInMailInput {
            link: &link,
            minutes: INVITE_TTL_SECS / 60,
            recipient: &email,
            invitation: Some(Invitation {
                workspace: &workspace.name,
                invited_by: &actor.name,
            }),
        },
    );
    match mailer.send(message).await {
        Ok(SendOutcome::Sent { .. }) => {}
        // Nothing was sent, so nothing may stay open: the invitation and its
        // link are withdrawn rather than left for a mail that never went.
        Ok(SendOutcome::NotConfigured) => {
            let _ = store::revoke_invite(&*db, &workspace.id, &id, &now_text).await;
            return Err(Problem::new(&MAIL_NOT_CONFIGURED));
        }
        Err(error) => {
            let _ = store::revoke_invite(&*db, &workspace.id, &id, &now_text).await;
            return Err(mail_failed(error));
        }
    }
    let created = store::invite(&*db, &workspace.id, &id)
        .await
        .map_err(database)?
        .ok_or_else(Problem::internal)?;
    Ok((StatusCode::CREATED, Json(InviteView::from(created))).into_response())
}

/// `GET /invites` — the open invitations. Owners and admins only: the list
/// names addresses.
pub(crate) async fn invites(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let db = port(state.ctx.ports.db.clone())?;
    let (_, now) = now_text(&state)?;
    let open = store::open_invites(&*db, &workspace.id, &now)
        .await
        .map_err(database)?;
    Ok(Json(open.into_iter().map(InviteView::from).collect::<Vec<_>>()).into_response())
}

/// `DELETE /invites/{id}` — withdraw an open invitation; its link stops
/// working.
pub(crate) async fn revoke_invite(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if !actor.may_administer() {
        return Err(forbidden());
    }
    let db = port(state.ctx.ports.db.clone())?;
    let Some(found) = store::invite(&*db, &workspace.id, &id)
        .await
        .map_err(database)?
    else {
        return Err(Problem::new(&NOT_FOUND));
    };
    // An admin may not withdraw what only an owner could have sent.
    if found.role == Role::Owner && actor.role != Role::Owner {
        return Err(forbidden());
    }
    let (_, now) = now_text(&state)?;
    if !store::revoke_invite(&*db, &workspace.id, &id, &now)
        .await
        .map_err(database)?
    {
        return Err(Problem::new(&NOT_FOUND));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// The workspace's name and avatar

/// The workspace as its profile routes answer it.
#[derive(Debug, Serialize)]
struct WorkspaceProfile {
    id: String,
    name: String,
    avatar_url: Option<String>,
}

async fn workspace_profile(
    db: &dyn cratefield_core::Database,
    workspace_id: &str,
) -> Result<WorkspaceProfile, Problem> {
    let workspace = store::workspace(db, workspace_id)
        .await
        .map_err(database)?
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    Ok(WorkspaceProfile {
        avatar_url: AvatarRef::of(workspace.avatar.as_deref()).map(|avatar| avatar.url),
        id: workspace.id,
        name: workspace.name,
    })
}

/// The workspace the session names, after checking that its member may
/// change how the workspace is presented: an owner or admin.
async fn administered(
    state: &ModuleState,
    headers: &HeaderMap,
) -> Result<store::Workspace, Problem> {
    require_same_origin(headers)?;
    let (workspace, actor, is_owner) = session_scope(state, headers).await?;
    if !(is_owner || actor.may_administer()) {
        return Err(forbidden());
    }
    Ok(workspace)
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspacePatch {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeleteBody {
    /// The workspace's name, typed by the owner: the guard against a
    /// deletion sent by mistake.
    confirm: String,
}

/// `DELETE /current` — delete the workspace (ADR 0004, "Deleting a
/// workspace"). Owners only, session only, same-origin.
///
/// The order is deliberate. Everything that can be refused is checked first:
/// the owner, the typed name, and that the person keeps another workspace to
/// sign in to. The next session is sealed before anything is destroyed. Then
/// the content and the credentials other modules hold are erased through the
/// [`WorkspaceErasure`] seam, which is idempotent, so a failure there leaves
/// the workspace whole for another attempt. Last, this module's own rows go
/// in one batch and the workspace is tombstoned, which every read refuses.
pub(crate) async fn delete_workspace(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<DeleteBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, actor, _) = session_scope(&state, &headers).await?;
    if actor.role != Role::Owner {
        return Err(forbidden());
    }
    if body.confirm != workspace.name {
        return Err(Problem::new(&CONFIRM_MISMATCH));
    }
    let db = port(state.ctx.ports.db.clone())?;
    let erasure = port(state.erasure.clone())?;
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;

    // The other workspaces this person signs in to, decided before this one
    // loses its identities. The oldest is where they land.
    let addresses = store::email_identities(&*db, &workspace.id, &actor.user_id)
        .await
        .map_err(database)?;
    let next = store::memberships_for(&*db, &addresses)
        .await
        .map_err(database)?
        .into_iter()
        .filter(|found| found.workspace_id != workspace.id)
        .min_by(|a, b| a.workspace_created_at.cmp(&b.workspace_created_at))
        .ok_or_else(|| Problem::new(&ONLY_WORKSPACE))?;
    let value = flow::seal(
        &*signer,
        flow::SESSION_PURPOSE,
        &Session {
            workspace_id: next.workspace_id.clone(),
            user_id: next.user_id.clone(),
            exp: clock.now().unix_timestamp() + flow::SESSION_TTL_SECS,
        },
    )
    .ok_or_else(Problem::internal)?;

    let (now, now_text) = now_text(&state)?;
    // Every member id this workspace ever had: each one's personal space is
    // this workspace's content, removed members' included.
    let members = store::every_member_id(&*db, &workspace.id)
        .await
        .map_err(database)?;
    let mut spaces = vec!["shared".to_owned()];
    spaces.extend(members.iter().map(|id| format!("personal:{id}")));
    spaces.extend(
        store::channel_keys(&*db, &workspace.id)
            .await
            .map_err(database)?
            .into_iter()
            .map(|key| format!("channel:{key}")),
    );
    erasure
        .erase(&*db, now, &workspace.id, &spaces)
        .await
        .map_err(|_| Problem::internal())?;
    store::delete_workspace_rows(&*db, &workspace.id, &now_text)
        .await
        .map_err(database)?;
    // The workspace's own avatar image goes too. Best effort, like the
    // avatar routes: the row that named it is already gone.
    if let (Some(blob), Some(avatar)) = (state.ctx.ports.blob.clone(), workspace.avatar.as_deref())
    {
        profile::forget_avatar(&*blob, &workspace.id, Some(avatar), None).await;
    }

    let mut response = Json(serde_json::json!({
        "deleted": workspace.id,
        "next": { "id": next.workspace_id, "name": next.workspace_name },
    }))
    .into_response();
    response.headers_mut().append(
        cratefield_core::axum::http::header::SET_COOKIE,
        flow::set_cookie(flow::SESSION_COOKIE, &value, flow::SESSION_TTL_SECS)
            .ok_or_else(Problem::internal)?,
    );
    Ok(response)
}

/// `PATCH /current` — rename the workspace. Owners and admins only; the
/// name is 1–64 visible characters, trimmed.
pub(crate) async fn rename_workspace(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<WorkspacePatch>,
) -> Result<Response, Problem> {
    let workspace = administered(&state, &headers).await?;
    let raw = body.name.ok_or_else(profile::nothing_to_change)?;
    let name = profile::clean_name(&raw).map_err(profile::name_problem)?;
    let db = port(state.ctx.ports.db.clone())?;
    profile::set_workspace_name(&*db, &workspace.id, &name)
        .await
        .map_err(database)?;
    Ok(Json(workspace_profile(&*db, &workspace.id).await?).into_response())
}

/// `PUT /current/avatar` — the workspace's avatar, as the raw image body.
/// Owners and admins only. The image is checked by its bytes, rewritten
/// without metadata and stored under a fresh id; the one it replaces is
/// deleted.
pub(crate) async fn put_workspace_avatar(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    body: cratefield_core::axum::body::Bytes,
) -> Result<Response, Problem> {
    let workspace = administered(&state, &headers).await?;
    let blob = state
        .ctx
        .ports
        .blob
        .clone()
        .ok_or_else(profile::avatars_unavailable)?;
    let declared = headers
        .get(cratefield_core::axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let image = profile::clean_image(&body, declared).map_err(profile::image_problem)?;
    let id_gen = port(state.ctx.ports.id_gen.clone())?;
    let db = port(state.ctx.ports.db.clone())?;
    let id = profile::store_avatar(&*blob, &workspace.id, &id_gen.ulid(), &image).await?;
    let previous = profile::set_workspace_avatar(&*db, &workspace.id, Some(&id))
        .await
        .map_err(database)?;
    profile::forget_avatar(&*blob, &workspace.id, previous.as_deref(), Some(&id)).await;
    Ok(Json(workspace_profile(&*db, &workspace.id).await?).into_response())
}

/// `DELETE /current/avatar` — back to the initials. Owners and admins only.
pub(crate) async fn delete_workspace_avatar(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let workspace = administered(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let previous = profile::set_workspace_avatar(&*db, &workspace.id, None)
        .await
        .map_err(database)?;
    if let Some(blob) = state.ctx.ports.blob.clone() {
        profile::forget_avatar(&*blob, &workspace.id, previous.as_deref(), None).await;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// More than one workspace

#[derive(Debug, Serialize)]
struct WorkspaceItem {
    id: String,
    name: String,
    /// The same-origin URL of the workspace's avatar, or `null`.
    avatar_url: Option<String>,
    role: &'static str,
    current: bool,
}

/// `GET /` — the workspaces the signed-in person can switch to: every
/// workspace where an email address they sign in with here is a live
/// member's identity, plus this one.
pub(crate) async fn list_workspaces(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (workspace, member, _) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let addresses = store::email_identities(&*db, &workspace.id, &member.user_id)
        .await
        .map_err(database)?;
    let mut items: Vec<WorkspaceItem> = store::memberships_for(&*db, &addresses)
        .await
        .map_err(database)?
        .into_iter()
        .map(|found| WorkspaceItem {
            avatar_url: AvatarRef::of(found.workspace_avatar.as_deref()).map(|avatar| avatar.url),
            current: found.workspace_id == workspace.id,
            id: found.workspace_id,
            name: found.workspace_name,
            role: found.role.as_str(),
        })
        .collect();
    items.dedup_by(|a, b| a.id == b.id);
    if !items.iter().any(|item| item.current) {
        items.insert(
            0,
            WorkspaceItem {
                avatar_url: AvatarRef::of(workspace.avatar.as_deref()).map(|avatar| avatar.url),
                id: workspace.id,
                name: workspace.name,
                role: member.role.as_str(),
                current: true,
            },
        );
    }
    Ok(Json(items).into_response())
}

/// One workspace a signed-in person may act in, with the member id they hold
/// there. The OAuth consent page (`livingbrain-tokens`) offers these as its
/// workspace picker and mints the approved token for the chosen pair, so the
/// picker never has to switch the browser's session cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceChoice {
    pub workspace_id: String,
    pub name: String,
    /// The member's id **in that workspace** — not necessarily the id the
    /// caller holds in the workspace they are signed in to.
    pub user_id: String,
    pub role: Role,
}

/// Every workspace `user_id` in `workspace_id` may act in: the same list
/// [`list_workspaces`] answers (every workspace where an email identity of
/// theirs is a live member), with the workspace they are signed in to always
/// present. Ordered as the switcher orders it, current one first when it is
/// the only one.
///
/// # Errors
///
/// [`DbError`] when a lookup fails.
pub async fn workspace_choices(
    db: &dyn cratefield_core::Database,
    workspace_id: &str,
    user_id: &str,
) -> Result<Vec<WorkspaceChoice>, cratefield_core::DbError> {
    let addresses = store::email_identities(db, workspace_id, user_id).await?;
    let mut items: Vec<WorkspaceChoice> = store::memberships_for(db, &addresses)
        .await?
        .into_iter()
        .map(|found| WorkspaceChoice {
            workspace_id: found.workspace_id,
            name: found.workspace_name,
            user_id: found.user_id,
            role: found.role,
        })
        .collect();
    items.dedup_by(|a, b| a.workspace_id == b.workspace_id);
    if !items.iter().any(|item| item.workspace_id == workspace_id)
        && let (Some(workspace), Some(member)) = (
            store::workspace(db, workspace_id).await?,
            store::member(db, workspace_id, user_id).await?,
        )
    {
        items.insert(
            0,
            WorkspaceChoice {
                workspace_id: workspace.id,
                name: workspace.name,
                user_id: member.user_id,
                role: member.role,
            },
        );
    }
    Ok(items)
}

/// The most workspaces one person may own: past any real need, and short of
/// a loop minting rows.
pub(crate) const MAX_OWNED_WORKSPACES: usize = 20;

/// A new workspace needs an address to reach it by.
const NO_EMAIL: ProblemDef = ProblemDef {
    slug: "workspaces/no-email",
    status: StatusCode::CONFLICT,
    title: "Sign in with your email address first",
    description: "A new workspace is reached through the email address you sign in with, \
                  and this sign-in has none. Sign in with your email address and try again.",
};

/// The person already owns as many workspaces as one may.
const TOO_MANY_WORKSPACES: ProblemDef = ProblemDef {
    slug: "workspaces/too-many",
    status: StatusCode::CONFLICT,
    title: "You own as many workspaces as one person may",
    description: "Delete or hand over one you no longer use first.",
};

#[derive(Debug, Deserialize)]
pub(crate) struct CreateBody {
    name: String,
}

/// `POST /` — a new workspace, owned by the signed-in person, who is moved
/// into it. Session only and same-origin, like the switcher.
///
/// The person is the same person through their email address: the new
/// workspace gets a member row of its own (`usr_…`, owner, the name they go
/// by here) and the same addresses as identities, so the switcher lists it
/// and `/switch` reaches it — and so a sign-in link to that address can
/// open it, exactly as for a workspace created at sign-up.
pub(crate) async fn create_workspace(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, member, _) = session_scope(&state, &headers).await?;
    let name = profile::clean_name(&body.name).map_err(profile::name_problem)?;
    let db = port(state.ctx.ports.db.clone())?;
    let addresses = store::email_identities(&*db, &workspace.id, &member.user_id)
        .await
        .map_err(database)?;
    if addresses.is_empty() {
        return Err(Problem::new(&NO_EMAIL));
    }
    let mut owned: Vec<String> = store::memberships_for(&*db, &addresses)
        .await
        .map_err(database)?
        .into_iter()
        .filter(|found| found.role == Role::Owner)
        .map(|found| found.workspace_id)
        .collect();
    owned.dedup();
    if owned.len() >= MAX_OWNED_WORKSPACES {
        return Err(Problem::new(&TOO_MANY_WORKSPACES));
    }

    let id_gen = port(state.ctx.ports.id_gen.clone())?;
    let (_, now) = now_text(&state)?;
    let workspace_id = new_workspace_id(&*id_gen);
    let owner_id = new_user_id(&*id_gen);
    store::ensure_workspace(&*db, &workspace_id, &name, &owner_id, &now)
        .await
        .map_err(database)?;
    for address in &addresses {
        store::link_identity(&*db, &workspace_id, "email", address, &owner_id, &now)
            .await
            .map_err(database)?;
    }
    // The owner row: `upsert_member` makes the workspace's owner an owner.
    store::upsert_member(
        &*db,
        &workspace_id,
        &owner_id,
        store::MemberFields {
            name: Some(member.name.as_str()),
            ..store::MemberFields::default()
        },
        &now,
    )
    .await
    .map_err(database)?;

    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let value = flow::seal(
        &*signer,
        flow::SESSION_PURPOSE,
        &Session {
            workspace_id: workspace_id.clone(),
            user_id: owner_id,
            exp: clock.now().unix_timestamp() + flow::SESSION_TTL_SECS,
        },
    )
    .ok_or_else(Problem::internal)?;
    let mut response = (
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": workspace_id, "name": name, "role": "owner" })),
    )
        .into_response();
    response.headers_mut().append(
        cratefield_core::axum::http::header::SET_COOKIE,
        flow::set_cookie(flow::SESSION_COOKIE, &value, flow::SESSION_TTL_SECS)
            .ok_or_else(Problem::internal)?,
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
pub(crate) struct SwitchBody {
    workspace_id: String,
}

/// `POST /switch` — seal a session for the member the signed-in person is
/// in another workspace. Refused unless an email address they sign in with
/// *here* is a live member's identity *there*: a workspace is never reached
/// by naming it.
pub(crate) async fn switch(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Json(body): Json<SwitchBody>,
) -> Result<Response, Problem> {
    require_same_origin(&headers)?;
    let (workspace, member, _) = session_scope(&state, &headers).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let addresses = store::email_identities(&*db, &workspace.id, &member.user_id)
        .await
        .map_err(database)?;
    let target = store::memberships_for(&*db, &addresses)
        .await
        .map_err(database)?
        .into_iter()
        .find(|found| found.workspace_id == body.workspace_id.trim())
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    let signer = port(state.ctx.ports.signer.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;
    let value = flow::seal(
        &*signer,
        flow::SESSION_PURPOSE,
        &Session {
            workspace_id: target.workspace_id.clone(),
            user_id: target.user_id,
            exp: clock.now().unix_timestamp() + flow::SESSION_TTL_SECS,
        },
    )
    .ok_or_else(Problem::internal)?;
    let mut response =
        Json(serde_json::json!({ "workspace_id": target.workspace_id })).into_response();
    response.headers_mut().append(
        cratefield_core::axum::http::header::SET_COOKIE,
        flow::set_cookie(flow::SESSION_COOKIE, &value, flow::SESSION_TTL_SECS)
            .ok_or_else(Problem::internal)?,
    );
    Ok(response)
}

/// Accepts an open invitation while a sign-in link is spent: called by
/// `POST /email/verify` for an address that is not yet a member of the
/// link's workspace. Answers the new member's id, or `None` when there is no
/// open invitation for the address.
pub(crate) async fn accept_on_sign_in(
    db: &dyn cratefield_core::Database,
    id_gen: &dyn cratefield_core::IdGen,
    workspace_id: &str,
    email: &str,
    now: &str,
) -> Result<Option<(String, Role)>, Problem> {
    let user_id = format!("usr_{}", id_gen.ulid());
    let Some(role) = store::accept_invite(db, workspace_id, email, &user_id, now)
        .await
        .map_err(database)?
    else {
        return Ok(None);
    };
    store::link_identity(db, workspace_id, "email", email, &user_id, now)
        .await
        .map_err(database)?;
    Ok(Some((user_id, role)))
}
