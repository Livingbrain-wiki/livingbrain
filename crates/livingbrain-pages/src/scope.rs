//! Which page scope one access scope is, and which scopes one asker may read.
//!
//! A page is `(scope, slug)` with no tenant column, so a page scope has to
//! name its workspace itself — see [`page_scope`]. [`page_scopes_for`] turns
//! an asker and a location into the ordered set of page scopes that asker may
//! read, and it is the **only** way this crate names one. Every caller reads
//! through it and none accepts a scope argument, so a caller cannot ask for a
//! page it was not granted.
//!
//! This lives here rather than in the MCP server because the Slack agent
//! (issue #123) has to answer a person from the same pages, and two copies of
//! the naming rule would be two answer keys.

use livingbrain_access::{Location, MembershipView, Scope, UserId, scopes_for};
use sha2::{Digest, Sha256};

/// The page-store scope one access scope maps to, inside `workspace_id`.
///
/// **The workspace is folded in on purpose.** A page is `(scope, slug)` with
/// no tenant column, so two workspaces sharing a scope name would share every
/// page in it, and two stores cannot be relied on to disagree about that.
/// A SHA-256 over the workspace and the scope, hex-truncated to 64 bits: the
/// ids behind them are Slack user ids and ULIDs, which are neither slug-safe
/// nor worth putting in a citation.
pub fn page_scope(workspace_id: &str, scope: &Scope) -> String {
    let (prefix, id) = match scope {
        Scope::Shared => ("shared", String::new()),
        Scope::Channel(id) => ("channel", id.to_string()),
        Scope::User(id) => ("user", id.to_string()),
    };
    let key = format!("{workspace_id}\u{1f}{prefix}\u{1f}{id}");
    let digest = Sha256::digest(key.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{prefix}-{hex}")
}

/// The page scopes one asker may read at one location, most specific first.
///
/// The grant is [`scopes_for`]'s: the asker's own memory before the shared
/// one, and a private channel's only for its members. The ordering is what
/// makes "the first hit wins" mean "the most private page wins", so it is
/// part of the contract rather than a presentation detail.
pub fn page_scopes_for<M: MembershipView + ?Sized>(
    workspace_id: &str,
    user_id: &str,
    location: Location,
    memberships: &M,
) -> Vec<String> {
    let granted = scopes_for(&UserId::new(user_id.to_owned()), location, memberships);
    let mut ranked: Vec<(u8, String)> = granted
        .scopes()
        .map(|scope| (specificity(scope), page_scope(workspace_id, scope)))
        .collect();
    ranked.sort();
    ranked.dedup();
    ranked.into_iter().map(|(_, scope)| scope).collect()
}

/// Most specific first: the asker's own memory, then shared, then a channel.
fn specificity(scope: &Scope) -> u8 {
    match scope {
        Scope::User(_) => 0,
        Scope::Shared => 1,
        Scope::Channel(_) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livingbrain_access::ChannelMemberships;

    #[test]
    fn a_page_scope_folds_the_workspace_in() {
        let scope = Scope::User(UserId::new("U1".to_owned()));
        assert_eq!(page_scope("ws", &scope), page_scope("ws", &scope));
        assert_ne!(page_scope("ws", &scope), page_scope("other", &scope));
        assert!(page_scope("ws", &scope).starts_with("user-"));
        assert_eq!(page_scope("ws", &Scope::Shared).len(), "shared-".len() + 16);
    }

    #[test]
    fn a_dm_sees_its_own_memory_then_shared() {
        let scopes = page_scopes_for("ws", "U1", Location::Dm, &ChannelMemberships::new());
        assert_eq!(scopes.len(), 2);
        assert!(scopes[0].starts_with("user-"));
        assert!(scopes[1].starts_with("shared-"));
        assert_eq!(
            scopes[0],
            page_scope("ws", &Scope::User(UserId::new("U1".to_owned())))
        );
    }
}
