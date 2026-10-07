//! Acceptance tests for the access model (issue #8).

use livingbrain_access::{
    Approval, ChannelId, ChannelMemberships, ConnectionGrant, ConnectionUse, Location,
    MembershipView, MemoryIndex, MemoryItem, Scope, ScopeParseError, UserId, connection_for_write,
    scopes_for,
};
use proptest::prelude::*;

proptest! {
    #[test]
    fn recall_respects_access_table(
        members in prop::collection::vec(prop::collection::vec(any::<bool>(), 5), 5),
        item_specs in prop::collection::vec((0u8..3u8, 0usize..5, 0usize..5), 0..=40),
        asker_idx in 0usize..5,
        loc_kind in 0u8..3u8,
        loc_chan_idx in 0usize..5,
    ) {
        let users: Vec<UserId> = (0..5).map(|i| UserId::new(format!("u{i}"))).collect();
        let channels: Vec<ChannelId> = (0..5).map(|i| ChannelId::new(format!("c{i}"))).collect();

        let mut memberships = ChannelMemberships::new();
        for (ci, row) in members.iter().enumerate() {
            let chan_members: Vec<UserId> = row.iter().enumerate()
                .filter(|&(_, &m)| m)
                .map(|(ui, _)| users[ui].clone())
                .collect();
            memberships.sync_channel(channels[ci].clone(), chan_members);
        }
        let asker = users[asker_idx].clone();

        let mut index = MemoryIndex::new();
        let mut item_scopes: std::collections::HashMap<String, Scope> =
            std::collections::HashMap::new();
        for (i, (kind, ci, ui)) in item_specs.iter().enumerate() {
            let scope = match kind {
                0 => Scope::Shared,
                1 => Scope::Channel(channels[*ci % channels.len()].clone()),
                _ => Scope::User(users[*ui % users.len()].clone()),
            };
            let id = format!("item-{i}");
            item_scopes.insert(id.clone(), scope.clone());
            index.insert(MemoryItem { id, scope, content: "query".to_owned() });
        }

        let location = match loc_kind {
            0 => Location::PublicChannel,
            1 => Location::PrivateChannel(channels[loc_chan_idx % channels.len()].clone()),
            _ => Location::Dm,
        };
        let scope_set = scopes_for(&asker, location.clone(), &memberships);

        // Shared is always present.
        prop_assert!(scope_set.contains(&Scope::Shared));

        match &location {
            Location::PublicChannel => {
                for ch in &channels {
                    prop_assert!(!scope_set.contains(&Scope::Channel(ch.clone())));
                }
                for u in &users {
                    prop_assert!(!scope_set.contains(&Scope::User(u.clone())));
                }
            }
            Location::PrivateChannel(ch) => {
                // Channel scope only if the asker is a member; never another
                // channel or any user scope.
                assert_eq!(
                    scope_set.contains(&Scope::Channel(ch.clone())),
                    memberships.is_member(ch, &asker)
                );
                for other in &channels {
                    if other != ch {
                        prop_assert!(!scope_set.contains(&Scope::Channel(other.clone())));
                    }
                }
                for u in &users { prop_assert!(!scope_set.contains(&Scope::User(u.clone()))); }
            }
            Location::Dm => {
                // Asker's own user scope, never another user's; every channel
                // in the set is one the asker is a member of.
                prop_assert!(scope_set.contains(&Scope::User(asker.clone())));
                for u in &users {
                    if u != &asker { prop_assert!(!scope_set.contains(&Scope::User(u.clone()))); }
                }
                for ch in &channels {
                    if scope_set.contains(&Scope::Channel(ch.clone())) {
                        prop_assert!(memberships.is_member(ch, &asker));
                    }
                }
            }
        }

        // Every cited item's scope is in the ScopeSet.
        let answer = index.recall(&scope_set, "query");
        for id in &answer.citations {
            let scope = item_scopes.get(id).expect("cited id must exist");
            prop_assert!(
                scope_set.contains(scope),
                "cited item {id} scope {scope} not in ScopeSet"
            );
        }
    }

    #[test]
    fn scope_display_fromstr_roundtrips(kind in 0u8..3u8, id in "[a-z]{1,5}") {
        let scope = match kind {
            0 => Scope::Shared,
            1 => Scope::Channel(ChannelId::new(id.clone())),
            _ => Scope::User(UserId::new(id.clone())),
        };
        let s = scope.to_string();
        let parsed: Scope = s.parse().expect("round-trip must parse");
        prop_assert_eq!(parsed, scope);
    }
}

#[test]
fn scope_parse_table() {
    let ok = |s: &str, expected: Scope| assert_eq!(s.parse::<Scope>().unwrap(), expected);
    let err =
        |s: &str, expected: ScopeParseError| assert_eq!(s.parse::<Scope>().unwrap_err(), expected);
    ok("shared", Scope::Shared);
    ok(
        "channel:c1",
        Scope::Channel(ChannelId::new("c1".to_owned())),
    );
    ok("user:u1", Scope::User(UserId::new("u1".to_owned())));
    err("", ScopeParseError::Malformed);
    err("channel:", ScopeParseError::EmptyId);
    err("user:", ScopeParseError::EmptyId);
    err("bogus", ScopeParseError::Malformed);
    err("group:g1", ScopeParseError::Malformed);
}

#[test]
fn leaving_private_channel_removes_its_memory_from_dms() {
    let alice = UserId::new("alice".to_owned());
    let bob = UserId::new("bob".to_owned());
    let chan_c = ChannelId::new("C".to_owned());

    let mut memberships = ChannelMemberships::new();
    memberships.sync_channel(chan_c.clone(), [alice.clone(), bob.clone()]);

    let mut index = MemoryIndex::new();
    index.insert(MemoryItem {
        id: "chan-note".to_owned(),
        scope: Scope::Channel(chan_c.clone()),
        content: "secret".to_owned(),
    });
    index.insert(MemoryItem {
        id: "shared-note".to_owned(),
        scope: Scope::Shared,
        content: "secret".to_owned(),
    });

    // Alice is in C: DM recall cites C's item.
    let scopes = scopes_for(&alice, Location::Dm, &memberships);
    assert!(
        index
            .recall(&scopes, "secret")
            .citations
            .contains(&"chan-note".to_owned())
    );

    // Alice leaves C: DM recall no longer cites it, shared still visible.
    memberships.sync_channel(chan_c.clone(), [bob]);
    let after = scopes_for(&alice, Location::Dm, &memberships);
    assert!(!after.contains(&Scope::Channel(chan_c)));
    let answer = index.recall(&after, "secret");
    assert!(!answer.citations.contains(&"chan-note".to_owned()));
    assert!(answer.citations.contains(&"shared-note".to_owned()));
}

#[test]
fn connection_borrowing() {
    let alice = UserId::new("alice".to_owned());
    let bob = UserId::new("bob".to_owned());

    // Own: asker is the owner — always grants, even on Deny (own connection).
    assert_eq!(connection_for_write(&alice, &alice), ConnectionUse::Own);
    assert_eq!(
        ConnectionUse::Own.resolve(Approval::Deny),
        ConnectionGrant::Granted
    );

    // Teammate: needs approval; deny refuses, approve grants.
    let needs = connection_for_write(&alice, &bob);
    assert_eq!(needs, ConnectionUse::NeedsApproval { owner: bob.clone() });
    assert_eq!(
        needs.clone().resolve(Approval::Deny),
        ConnectionGrant::Refused { owner: bob.clone() }
    );
    assert_eq!(needs.resolve(Approval::Approve), ConnectionGrant::Granted);
}
