//! Who may read and write which part of a workspace's memory (ADR 0004): the
//! one authorization function every route, tool and query goes through.
//!
//! An [`AccessPlan`] is built once per request from the asker — a member or
//! an agent account, with the credential's scope list — and the grants stored
//! for them. Everything a route asks goes through it:
//!
//! - [`AccessPlan::filters`] — the page scopes to read and, for each, which
//!   slugs. Listings and search put the filter **in the query**, before any
//!   ranking or `LIMIT`, so nothing ungranted is counted, ranked, returned or
//!   handed to a model.
//! - [`AccessPlan::allows`] — one yes/no for one resource, for a read by slug,
//!   a write, a backlink or a citation.
//!
//! ## The rule
//!
//! The decision is `cratefield_module_orgs::grants::decide`: deny-by-default,
//! an applicable deny wins, credentials only narrow. What this module adds is
//! the venture's default reach, as implicit grants evaluated by the same rule:
//!
//! | Principal | Implicit grant |
//! |---|---|
//! | every member | `admin` on their own personal space |
//! | owner, admin | `admin` on the shared space |
//! | member | `write` on the shared space |
//! | viewer | `read` on the shared space |
//! | agent account | nothing |
//!
//! With no stored grants this is exactly the access the wiki had before ADR
//! 0004: your own memory and the shared one. A stored **deny** never takes a
//! member's own personal space away from them.
//!
//! ## Spaces, folders and paths
//!
//! A space is `shared`, `personal:<member id>` or `channel:<id>`, and maps to a
//! page scope through [`page_scope`](crate::page_scope) — so the workspace is
//! folded in and a grant can never name another workspace. A page's path is
//! its slug split on `--` (the coding-agents folder spelling: the slug
//! `coding-agents--claude--x` is the path `coding-agents/claude/x`); a fact's
//! is its subject split on `/`; a source's is `project/<name>`.

use std::collections::{BTreeMap, BTreeSet};

use cratefield_core::{Database, DbError};
use cratefield_module_orgs::grants::{
    self, ANY_KIND, Coverage, CredentialScope, Effect, Grant, Permission, Resource, Selector,
};
use livingbrain_access::{ChannelId, Scope, UserId};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;

use crate::scope::page_scope;

/// The resource kinds a grant can name.
pub const KIND_PAGES: &str = "pages";
/// Imported files and agent session logs (the source ledger).
pub const KIND_SOURCES: &str = "sources";
/// Temporal facts.
pub const KIND_FACTS: &str = "facts";

/// The shared space.
pub const SHARED: &str = "shared";

/// The personal space of one member.
#[must_use]
pub fn personal(member_id: &str) -> String {
    format!("personal:{member_id}")
}

/// The page scope a space names inside `workspace_id`, or `None` for a
/// string that is not a space.
#[must_use]
pub fn space_scope(workspace_id: &str, space: &str) -> Option<String> {
    let scope = if space == SHARED {
        Scope::Shared
    } else if let Some(id) = space.strip_prefix("personal:").filter(|id| !id.is_empty()) {
        Scope::User(UserId::new(id.to_owned()))
    } else {
        let id = space.strip_prefix("channel:").filter(|id| !id.is_empty())?;
        Scope::Channel(ChannelId::new(id.to_owned()))
    };
    Some(page_scope(workspace_id, &scope))
}

/// A page slug's folder path: its `--`-separated segments.
#[must_use]
pub fn slug_path(slug: &str) -> Vec<String> {
    slug.split("--")
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A fact subject's path: its `/`-separated segments.
#[must_use]
pub fn subject_path(subject: &str) -> Vec<String> {
    grants::split_path(subject)
}

/// A source's path: `project/<name>`, or the empty path for one filed under
/// no project (reachable only through a grant on the whole space).
#[must_use]
pub fn source_path(project: Option<&str>) -> Vec<String> {
    match project.filter(|project| !project.is_empty()) {
        Some(project) => vec!["project".to_owned(), project.to_owned()],
        None => Vec::new(),
    }
}

/// Whether a page `slug` exists in page scope `scope` — a metadata read, no
/// body opened, for a caller that has already decided the asker may read it
/// (the "who can see this page" panel).
///
/// # Errors
///
/// [`DbError`] when the read fails.
pub async fn page_exists(db: &dyn Database, scope: &str, slug: &str) -> Result<bool, DbError> {
    let rows = db
        .query(&cratefield_core::Statement::with_values(
            "SELECT 1 AS one FROM pages WHERE scope = ? AND slug = ?",
            vec![
                SeaValue::String(Some(Box::new(scope.to_owned()))),
                SeaValue::String(Some(Box::new(slug.to_owned()))),
            ],
        ))
        .await?;
    Ok(!rows.is_empty())
}

/// What kind of principal is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A person in the workspace, with their role (`owner`, `admin`,
    /// `member` or `viewer`).
    Member {
        /// The role, as stored.
        role: String,
    },
    /// An agent account, owned by a member.
    Agent {
        /// The member who answers for it.
        owner: String,
    },
}

/// Who is asking: a member or an agent account in one workspace, and the scope
/// list the credential was cut down to (`None` is the whole principal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The workspace, from the verified credential.
    pub workspace_id: String,
    /// The member id or the agent account id.
    pub id: String,
    /// Member or agent.
    pub kind: PrincipalKind,
    /// The credential's scope list, in the token vocabulary (see
    /// [`credential_scopes`]).
    pub credential_scopes: Option<Vec<String>>,
}

impl Principal {
    /// The principal strings stored grants are matched against: the member
    /// and their role group, or the agent account.
    #[must_use]
    pub fn grant_principals(&self) -> Vec<String> {
        match &self.kind {
            PrincipalKind::Member { role } => {
                vec![member_principal(&self.id), format!("role:{role}")]
            }
            PrincipalKind::Agent { .. } => vec![agent_principal(&self.id)],
        }
    }
}

/// The grant principal string for a member.
#[must_use]
pub fn member_principal(id: &str) -> String {
    format!("member:{id}")
}

/// The grant principal string for an agent account.
#[must_use]
pub fn agent_principal(id: &str) -> String {
    format!("agent:{id}")
}

/// Reads a credential's scope list into selectors. The vocabulary:
///
/// - `shared`, `user:<id>`, `channel:<id>` — the pre-ADR-0004 names, each the
///   whole space at every permission, so every existing token keeps exactly
///   its reach;
/// - `shared/<path>`, `personal:<id>[/<path>]`, `channel:<id>[/<path>]` — a
///   space or a folder in it;
/// - a `read:` prefix on any of them — the same reach, read-only.
///
/// An entry that parses as none of these reaches nothing: a typo narrows, it
/// never widens.
#[must_use]
pub fn credential_scopes(list: &[String]) -> Vec<CredentialScope> {
    list.iter()
        .filter_map(|entry| {
            let (permission, rest) = match entry.strip_prefix("read:") {
                Some(rest) => (Permission::Read, rest),
                None => (Permission::Admin, entry.as_str()),
            };
            let (space, path) = match rest.split_once('/') {
                Some((space, path)) => (space, grants::split_path(path)),
                None => (rest, Vec::new()),
            };
            let space = match space.strip_prefix("user:") {
                Some(id) => personal(id),
                None => space.to_owned(),
            };
            if space != SHARED && !space.starts_with("personal:") && !space.starts_with("channel:")
            {
                return None;
            }
            let selector = Selector::new(ANY_KIND, space, path).ok()?;
            Some(CredentialScope {
                selector,
                permission,
            })
        })
        .collect()
}

/// The implicit grants a principal holds with nothing stored. See the
/// [module docs](self).
#[must_use]
pub fn implicit_grants(principal: &Principal) -> Vec<Grant> {
    let whole = |space: &str| Selector::new(ANY_KIND, space, Vec::new()).ok();
    let mut out = Vec::new();
    if let PrincipalKind::Member { role } = &principal.kind {
        if let Some(own) = whole(&personal(&principal.id)) {
            out.push(Grant::implicit(
                member_principal(&principal.id),
                own,
                Permission::Admin,
            ));
        }
        let shared = match role.as_str() {
            "owner" | "admin" => Some(Permission::Admin),
            "member" => Some(Permission::Write),
            "viewer" => Some(Permission::Read),
            // An unknown role reaches nothing beyond its own space.
            _ => None,
        };
        if let (Some(permission), Some(selector)) = (shared, whole(SHARED)) {
            out.push(Grant::implicit(
                format!("role:{role}"),
                selector,
                permission,
            ));
        }
    }
    out
}

/// One page scope to read, and which of its slugs.
///
/// `allow` and `deny` are slug **folder prefixes**: a prefix matches the slug
/// that is the folder itself and every slug under it on a `--` boundary, and
/// the empty prefix matches everything. A slug is readable when an allow
/// matches and no deny does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeFilter {
    /// The page-store scope.
    pub scope: String,
    /// The space it is (`shared`, `personal:<id>`, …).
    pub space: String,
    allow: Vec<String>,
    deny: Vec<String>,
}

fn prefix_matches(prefix: &str, slug: &str) -> bool {
    prefix.is_empty()
        || slug == prefix
        || slug
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("--"))
}

impl ScopeFilter {
    fn from_coverage(scope: String, space: String, coverage: &Coverage) -> Self {
        Self {
            scope,
            space,
            allow: coverage.allow.iter().map(|path| path.join("--")).collect(),
            deny: coverage.deny.iter().map(|path| path.join("--")).collect(),
        }
    }

    /// The whole scope, nothing excluded.
    #[must_use]
    pub fn whole(scope: impl Into<String>, space: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            space: space.into(),
            allow: vec![String::new()],
            deny: Vec::new(),
        }
    }

    /// Whether `slug` in this scope is readable.
    #[must_use]
    pub fn matches(&self, slug: &str) -> bool {
        !self.deny.iter().any(|prefix| prefix_matches(prefix, slug))
            && self.allow.iter().any(|prefix| prefix_matches(prefix, slug))
    }

    /// Whether every slug in the scope is readable.
    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.deny.is_empty() && self.allow.iter().any(String::is_empty)
    }

    /// Splits this filter into pieces whose predicates each bind at most
    /// `budget` values, so a statement over the pieces stays inside D1's
    /// bound-parameter ceiling.
    ///
    /// The pieces are **disjoint**: the allow list first loses every prefix
    /// an ancestor in the list already covers, and what is left matches no
    /// slug twice (two folder prefixes that both match one slug are one the
    /// ancestor of the other). So per-piece counts, and per-piece answers
    /// merged by key, add up to what the whole filter would have given. Each
    /// piece keeps only the denies that can touch its own allows, which is
    /// what lets a long allow list fit. A deny list that cannot be split
    /// (one allow with more denies under it than the budget) is returned
    /// whole, and the statement built from it is over budget: the caller's
    /// fake reports it.
    pub(crate) fn split_to_budget(
        &self,
        budget: usize,
        binds: &dyn Fn(&ScopeFilter) -> usize,
    ) -> Vec<ScopeFilter> {
        if self.is_whole() {
            return vec![self.clone()];
        }
        let mut allow: Vec<String> = Vec::new();
        for prefix in &self.allow {
            if allow.iter().any(|kept| prefix_matches(kept, prefix)) {
                continue;
            }
            allow.retain(|kept| !prefix_matches(prefix, kept));
            allow.push(prefix.clone());
        }
        Self {
            scope: self.scope.clone(),
            space: self.space.clone(),
            allow,
            deny: self.deny.clone(),
        }
        .split_allows(budget, binds)
    }

    fn split_allows(
        &self,
        budget: usize,
        binds: &dyn Fn(&ScopeFilter) -> usize,
    ) -> Vec<ScopeFilter> {
        if binds(self) <= budget || self.allow.len() <= 1 {
            return vec![self.clone()];
        }
        let (left, right) = self.allow.split_at(self.allow.len() / 2);
        let half = |allow: &[String]| Self {
            scope: self.scope.clone(),
            space: self.space.clone(),
            allow: allow.to_vec(),
            // A deny that is no ancestor or descendant of any of these allows
            // matches no slug they could reach, so it is dropped here.
            deny: self
                .deny
                .iter()
                .filter(|deny| {
                    allow
                        .iter()
                        .any(|a| prefix_matches(deny, a) || prefix_matches(a, deny))
                })
                .cloned()
                .collect(),
        };
        let mut pieces = half(left).split_allows(budget, binds);
        pieces.extend(half(right).split_allows(budget, binds));
        pieces
    }

    /// The SQL predicate for this filter over a table with `scope` and
    /// `slug` columns (optionally qualified by `alias`), and its binds — so
    /// a listing or a search applies the grant before it orders or limits.
    ///
    /// Prefixes are slugs (`[a-z0-9-]`), so neither `%` nor `_` can appear in
    /// a `LIKE` pattern built from one; they are still bound, never spliced.
    #[must_use]
    pub fn predicate(&self, alias: Option<&str>) -> (String, Vec<SeaValue>) {
        let column = |name: &str| match alias {
            Some(alias) => format!("{alias}.{name}"),
            None => name.to_owned(),
        };
        self.predicate_on(&column("scope"), &column("slug"))
    }

    /// [`ScopeFilter::predicate`] over named columns — the `[[link]]` table
    /// names its slugs `from_slug` and `to_slug`. The column names are the
    /// caller's literals, never input.
    #[must_use]
    pub fn predicate_on(&self, scope_col: &str, slug_col: &str) -> (String, Vec<SeaValue>) {
        let mut values = vec![SeaValue::String(Some(Box::new(self.scope.clone())))];
        let mut sql = format!("({scope_col} = ?");
        let matcher = |prefixes: &[String], values: &mut Vec<SeaValue>| -> String {
            prefixes
                .iter()
                .map(|prefix| {
                    values.push(SeaValue::String(Some(Box::new(prefix.clone()))));
                    values.push(SeaValue::String(Some(Box::new(format!("{prefix}--%")))));
                    format!("({slug_col} = ? OR {slug_col} LIKE ?)")
                })
                .collect::<Vec<_>>()
                .join(" OR ")
        };
        if !self.allow.iter().any(String::is_empty) {
            let allow = matcher(&self.allow, &mut values);
            sql.push_str(&format!(" AND ({allow})"));
        }
        let deny: Vec<String> = self
            .deny
            .iter()
            .filter(|prefix| !prefix.is_empty())
            .cloned()
            .collect();
        if self.deny.iter().any(String::is_empty) {
            sql.push_str(" AND 1 = 0");
        } else if !deny.is_empty() {
            let deny = matcher(&deny, &mut values);
            sql.push_str(&format!(" AND NOT ({deny})"));
        }
        sql.push(')');
        (sql, values)
    }
}

/// What one asker may reach, built once per request. See the
/// [module docs](self).
#[derive(Debug, Clone)]
pub struct AccessPlan {
    workspace_id: String,
    principal: Principal,
    principals: Vec<String>,
    grants: Vec<Grant>,
    credential: Option<Vec<CredentialScope>>,
    /// Space → page scope, for every space any applicable grant names.
    spaces: BTreeMap<String, String>,
    now: OffsetDateTime,
}

impl AccessPlan {
    /// The plan for `principal`: their implicit grants and every live grant
    /// stored for them.
    ///
    /// # Errors
    ///
    /// [`DbError`] when the grants cannot be read. The caller answers it with
    /// a refusal: no plan, no access.
    pub async fn load(
        db: &dyn Database,
        principal: &Principal,
        now: OffsetDateTime,
    ) -> Result<Self, DbError> {
        let names = principal.grant_principals();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let stored = grants::grants_for(db, &principal.workspace_id, &refs).await?;
        Ok(Self::from_grants(principal, stored, now))
    }

    /// The plan over grants already read — [`AccessPlan::load`]'s pure half.
    #[must_use]
    pub fn from_grants(principal: &Principal, stored: Vec<Grant>, now: OffsetDateTime) -> Self {
        let principals = principal.grant_principals();
        let own = match principal.kind {
            PrincipalKind::Member { .. } => Some(personal(&principal.id)),
            PrincipalKind::Agent { .. } => None,
        };
        // Spaces a stored deny can never take from this principal: a
        // member's own personal space, and — for an owner — the shared one,
        // so no admin can lock the owners out of the space they answer for
        // (or out of managing its grants).
        let mut untouchable: Vec<String> = own.iter().cloned().collect();
        if matches!(&principal.kind, PrincipalKind::Member { role } if role == "owner") {
            untouchable.push(SHARED.to_owned());
        }
        let mut grants = implicit_grants(principal);
        grants.extend(stored.into_iter().filter(|grant| {
            !(grant.effect == Effect::Deny
                && untouchable
                    .iter()
                    .any(|space| space == grant.selector.space()))
        }));
        let mut spaces = BTreeMap::new();
        for grant in &grants {
            if !principals.contains(&grant.principal) || !grant.is_live(now) {
                continue;
            }
            let space = grant.selector.space();
            if let Some(scope) = space_scope(&principal.workspace_id, space) {
                spaces.insert(space.to_owned(), scope);
            }
        }
        Self {
            workspace_id: principal.workspace_id.clone(),
            principal: principal.clone(),
            principals,
            grants,
            credential: principal
                .credential_scopes
                .as_deref()
                .map(credential_scopes),
            spaces,
            now,
        }
    }

    /// The asker.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The workspace.
    #[must_use]
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// The asker's own personal space's page scope, or `None` for an agent.
    #[must_use]
    pub fn own_scope(&self) -> Option<String> {
        match self.principal.kind {
            PrincipalKind::Member { .. } => {
                space_scope(&self.workspace_id, &personal(&self.principal.id))
            }
            PrincipalKind::Agent { .. } => None,
        }
    }

    /// The space a page scope is, if the plan knows it.
    #[must_use]
    pub fn space_of(&self, scope: &str) -> Option<&str> {
        self.spaces
            .iter()
            .find(|(_, known)| known.as_str() == scope)
            .map(|(space, _)| space.as_str())
    }

    fn principal_refs(&self) -> Vec<&str> {
        self.principals.iter().map(String::as_str).collect()
    }

    /// Whether the asker may do `permission` to the resource of `kind` at
    /// `path` in page scope `scope`. A scope the plan does not know is a no.
    #[must_use]
    pub fn allows(&self, kind: &str, scope: &str, path: &[String], permission: Permission) -> bool {
        let Some(space) = self.space_of(scope) else {
            return false;
        };
        let Ok(resource) = Resource::new(kind, space, path.to_vec()) else {
            return false;
        };
        grants::decide_with_credential(
            &self.grants,
            &self.principal_refs(),
            self.credential.as_deref(),
            &resource,
            permission,
            self.now,
        )
        .is_allowed()
    }

    /// [`AccessPlan::allows`] for a page by its slug.
    #[must_use]
    pub fn allows_page(&self, scope: &str, slug: &str, permission: Permission) -> bool {
        self.allows(KIND_PAGES, scope, &slug_path(slug), permission)
    }

    /// The scopes to read `kind` from at `permission`, each with its filter:
    /// the asker's own space first, then the shared space, then every other
    /// space a grant opens — so "the first hit wins" still means "the most
    /// private page wins". A space with nothing open is left out.
    #[must_use]
    pub fn filters(&self, kind: &str, permission: Permission) -> Vec<ScopeFilter> {
        let own = match self.principal.kind {
            PrincipalKind::Member { .. } => Some(personal(&self.principal.id)),
            PrincipalKind::Agent { .. } => None,
        };
        let rank = |space: &str| {
            if Some(space) == own.as_deref() {
                0
            } else if space == SHARED {
                1
            } else {
                2
            }
        };
        let mut ordered: Vec<(&String, &String)> = self.spaces.iter().collect();
        ordered.sort_by_key(|(space, _)| (rank(space), (*space).clone()));
        let refs = self.principal_refs();
        ordered
            .into_iter()
            .filter_map(|(space, scope)| {
                let coverage = grants::coverage(
                    &self.grants,
                    &refs,
                    self.credential.as_deref(),
                    kind,
                    space,
                    permission,
                    self.now,
                );
                (!coverage.is_empty())
                    .then(|| ScopeFilter::from_coverage(scope.clone(), space.clone(), &coverage))
            })
            .collect()
    }

    /// The page scopes [`AccessPlan::filters`] reads, in its order.
    #[must_use]
    pub fn scopes(&self, kind: &str, permission: Permission) -> Vec<String> {
        self.filters(kind, permission)
            .into_iter()
            .map(|filter| filter.scope)
            .collect()
    }

    /// Whether the asker may create or delete a grant on `selector` —
    /// `admin` over it, under the same rule, and never through a credential
    /// narrowed to less.
    #[must_use]
    pub fn may_grant(&self, selector: &Selector) -> bool {
        if self.credential.is_some() {
            return false;
        }
        grants::may_grant(&self.grants, &self.principal_refs(), selector, self.now)
    }

    /// The projects a project-limited sources grant opens in `scope` at
    /// read: every allowed path shaped `project/<name>`. A scope read whole
    /// answers through [`AccessPlan::filters`] instead.
    #[must_use]
    pub fn source_projects(&self, scope: &str) -> Vec<String> {
        let Some(space) = self.space_of(scope) else {
            return Vec::new();
        };
        let coverage = grants::coverage(
            &self.grants,
            &self.principal_refs(),
            self.credential.as_deref(),
            KIND_SOURCES,
            space,
            Permission::Read,
            self.now,
        );
        let mut names: Vec<String> = coverage
            .allow
            .iter()
            .filter_map(|path| match path.as_slice() {
                [first, name, ..] if first == "project" => Some(name.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every space the plan reaches at all.
    #[must_use]
    pub fn spaces(&self) -> BTreeSet<String> {
        self.spaces.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("in range")
    }

    fn member(id: &str, role: &str) -> Principal {
        Principal {
            workspace_id: "ws".to_owned(),
            id: id.to_owned(),
            kind: PrincipalKind::Member {
                role: role.to_owned(),
            },
            credential_scopes: None,
        }
    }

    fn agent(id: &str) -> Principal {
        Principal {
            workspace_id: "ws".to_owned(),
            id: id.to_owned(),
            kind: PrincipalKind::Agent {
                owner: "alice".to_owned(),
            },
            credential_scopes: None,
        }
    }

    fn stored(principal: &str, space: &str, path: &str, permission: Permission) -> Grant {
        Grant {
            id: "g".to_owned(),
            ..Grant::implicit(
                principal,
                Selector::new(KIND_PAGES, space, grants::split_path(path)).expect("valid"),
                permission,
            )
        }
    }

    fn scope(space: &str) -> String {
        space_scope("ws", space).expect("a space")
    }

    #[test]
    fn with_nothing_stored_a_member_reads_their_own_space_and_the_shared_one() {
        let plan = AccessPlan::from_grants(&member("alice", "member"), Vec::new(), now());
        let scopes = plan.scopes(KIND_PAGES, Permission::Read);
        assert_eq!(scopes, vec![scope("personal:alice"), scope(SHARED)]);
        assert!(plan.allows_page(&scope(SHARED), "x", Permission::Write));
        assert!(!plan.allows_page(&scope(SHARED), "x", Permission::Admin));
        assert!(!plan.allows_page(&scope("personal:bob"), "x", Permission::Read));
        // The same pages the pre-grant rule names, in the same order.
        let legacy = crate::page_scopes_for(
            "ws",
            "alice",
            livingbrain_access::Location::Dm,
            &livingbrain_access::ChannelMemberships::new(),
        );
        assert_eq!(scopes, legacy);
    }

    #[test]
    fn an_agent_reaches_nothing_until_granted() {
        let plan = AccessPlan::from_grants(&agent("agt_1"), Vec::new(), now());
        assert!(plan.scopes(KIND_PAGES, Permission::Read).is_empty());
        assert_eq!(plan.own_scope(), None);
        let plan = AccessPlan::from_grants(
            &agent("agt_1"),
            vec![stored(
                "agent:agt_1",
                "personal:alice",
                "coding-agents/claude",
                Permission::Write,
            )],
            now(),
        );
        let filters = plan.filters(KIND_PAGES, Permission::Read);
        assert_eq!(filters.len(), 1);
        assert!(filters[0].matches("coding-agents--claude--chi-infra--index"));
        assert!(filters[0].matches("coding-agents--claude"));
        assert!(!filters[0].matches("coding-agents--codex--x"));
        assert!(!filters[0].matches("coding-agents--claudette"));
        assert!(!filters[0].matches("private-note"));
        assert!(plan.allows_page(
            &scope("personal:alice"),
            "coding-agents--claude--x",
            Permission::Write
        ));
        assert!(!plan.allows_page(&scope(SHARED), "x", Permission::Read));
        assert!(
            plan.scopes(KIND_FACTS, Permission::Read).is_empty(),
            "a pages grant is not a facts grant"
        );
    }

    #[test]
    fn a_deny_carves_a_folder_out_but_never_the_owners_own_space() {
        let plan = AccessPlan::from_grants(
            &member("bob", "member"),
            vec![
                stored("member:bob", "personal:alice", "", Permission::Read),
                Grant {
                    effect: Effect::Deny,
                    ..stored("member:bob", "personal:alice", "diary", Permission::Read)
                },
                Grant {
                    effect: Effect::Deny,
                    ..stored("member:bob", "personal:bob", "", Permission::Read)
                },
            ],
            now(),
        );
        let alice = scope("personal:alice");
        assert!(plan.allows_page(&alice, "notes", Permission::Read));
        assert!(!plan.allows_page(&alice, "diary--today", Permission::Read));
        assert!(!plan.allows_page(&alice, "notes", Permission::Write));
        assert!(
            plan.allows_page(&scope("personal:bob"), "anything", Permission::Admin),
            "a deny on your own space is ignored"
        );
    }

    #[test]
    fn no_deny_locks_an_owner_out_of_the_shared_space() {
        let deny = Grant {
            effect: Effect::Deny,
            ..stored("role:owner", SHARED, "", Permission::Read)
        };
        let owner = AccessPlan::from_grants(&member("alice", "owner"), vec![deny.clone()], now());
        assert!(owner.allows_page(&scope(SHARED), "x", Permission::Admin));
        assert!(owner.may_grant(&Selector::new("*", SHARED, Vec::new()).expect("valid")));
        // The same deny does bind an admin.
        let admin_deny = Grant {
            principal: "role:admin".to_owned(),
            ..deny
        };
        let admin = AccessPlan::from_grants(&member("bob", "admin"), vec![admin_deny], now());
        assert!(!admin.allows_page(&scope(SHARED), "x", Permission::Read));
    }

    #[test]
    fn a_credential_narrows_and_the_legacy_names_keep_their_reach() {
        let mut alice = member("alice", "member");
        alice.credential_scopes = Some(vec!["shared".to_owned()]);
        let plan = AccessPlan::from_grants(&alice, Vec::new(), now());
        assert_eq!(
            plan.scopes(KIND_PAGES, Permission::Read),
            vec![scope(SHARED)]
        );
        assert!(plan.allows_page(&scope(SHARED), "x", Permission::Write));
        assert!(!plan.may_grant(&Selector::new("*", SHARED, Vec::new()).expect("valid")));

        alice.credential_scopes = Some(vec!["user:alice".to_owned(), "nonsense".to_owned()]);
        let plan = AccessPlan::from_grants(&alice, Vec::new(), now());
        assert_eq!(
            plan.scopes(KIND_PAGES, Permission::Read),
            vec![scope("personal:alice")]
        );

        alice.credential_scopes = Some(vec!["read:personal:alice/coding-agents".to_owned()]);
        let plan = AccessPlan::from_grants(&alice, Vec::new(), now());
        let own = scope("personal:alice");
        assert!(plan.allows_page(&own, "coding-agents--x", Permission::Read));
        assert!(!plan.allows_page(&own, "coding-agents--x", Permission::Write));
        assert!(!plan.allows_page(&own, "diary", Permission::Read));
        // A credential never widens what the grants hold.
        alice.credential_scopes = Some(vec!["personal:bob".to_owned()]);
        let plan = AccessPlan::from_grants(&alice, Vec::new(), now());
        assert!(plan.scopes(KIND_PAGES, Permission::Read).is_empty());
    }

    #[test]
    fn only_the_space_owner_and_its_admins_may_share_it() {
        let selector = |space: &str| Selector::new(KIND_PAGES, space, Vec::new()).expect("valid");
        let owner = AccessPlan::from_grants(&member("alice", "owner"), Vec::new(), now());
        assert!(owner.may_grant(&selector("personal:alice")));
        assert!(owner.may_grant(&selector(SHARED)));
        assert!(
            !owner.may_grant(&selector("personal:bob")),
            "a workspace owner cannot share somebody else's memory"
        );
        let plain = AccessPlan::from_grants(&member("carol", "member"), Vec::new(), now());
        assert!(!plain.may_grant(&selector(SHARED)));
        assert!(plain.may_grant(&selector("personal:carol")));
    }

    #[test]
    fn the_predicate_says_what_matches_says() {
        let filter = ScopeFilter {
            scope: "s".to_owned(),
            space: "personal:a".to_owned(),
            allow: vec!["coding-agents--claude".to_owned()],
            deny: vec!["coding-agents--claude--secret".to_owned()],
        };
        let (sql, values) = filter.predicate(Some("p"));
        assert_eq!(
            sql,
            "(p.scope = ? AND ((p.slug = ? OR p.slug LIKE ?)) AND NOT ((p.slug = ? OR p.slug LIKE ?)))"
        );
        assert_eq!(values.len(), 5);
        let whole = ScopeFilter::whole("s", SHARED);
        assert_eq!(whole.predicate(None).0, "(scope = ?)");
        assert!(whole.is_whole() && whole.matches("anything"));
    }

    #[test]
    fn split_pieces_are_disjoint_and_match_exactly_what_the_filter_matches() {
        let filter = ScopeFilter {
            scope: "s".to_owned(),
            space: SHARED.to_owned(),
            allow: (0..60)
                .map(|i| format!("f{i:02}"))
                .chain(["f01--x".to_owned()])
                .collect(),
            deny: vec!["f07--secret".to_owned(), "f30".to_owned()],
        };
        let binds = |f: &ScopeFilter| f.predicate(None).1.len();
        let pieces = filter.split_to_budget(20, &binds);
        assert!(pieces.len() > 1, "a 60-prefix filter must split");
        assert!(pieces.iter().all(|piece| binds(piece) <= 20));
        for slug in [
            "f01",
            "f01--x",
            "f01--y",
            "f07",
            "f07--secret",
            "f07--ok",
            "f30--a",
            "f59",
            "g1",
        ] {
            let hits = pieces.iter().filter(|piece| piece.matches(slug)).count();
            assert_eq!(hits, usize::from(filter.matches(slug)), "slug {slug}");
        }
    }
}
