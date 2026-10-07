//! The acceptance property test: Slack and Discord are equals.
//!
//! For a private Slack channel and a role-gated Discord guild channel with the
//! same membership, both `receive` to `Visibility::Private`, both `viewers`
//! map (through [`Links`]) to the same member set, and `scopes_for` and
//! `scope_of` agree across platforms. Hand-rolled over ~1000 random
//! membership subsets (fixed-seed xorshift); no proptest.

use std::collections::BTreeSet;

use livingbrain_channel::discord::{
    Discord, DiscordEvent, GuildMember, Overwrite, Role, Roster as DiscordRoster,
};
use livingbrain_channel::slack::{Roster as SlackRoster, Slack, SlackEvent};
use livingbrain_channel::{
    Channel, Identity, Links, Location, MemberId, Platform, Scope, Visibility, scope_of, scopes_for,
};
use serde_json::json;

const VIEW_CHANNEL: u64 = 1 << 10;
/// Administrator short-circuits `can_view`; the noise roles must never carry it.
const ADMINISTRATOR: u64 = 1 << 3;

/// A fixed-seed xorshift64 PRNG — deterministic across runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

/// A private Slack channel event for `channel` from `user`.
fn slack_event(channel: &str, user: &str) -> SlackEvent {
    serde_json::from_value(json!({
        "team_id": "T", "event": {
            "type": "message", "user": user, "channel": channel,
            "channel_type": "group", "text": "x", "ts": "1.1"
        }
    }))
    .expect("fixture")
}

/// A private Discord guild channel event for `channel` from `user`.
fn discord_event(channel: &str, user: &str) -> DiscordEvent {
    serde_json::from_value(json!({
        "message": {"id": "M", "channel_id": channel, "guild_id": "G",
            "author": {"id": user}, "content": "x", "mentions": []},
        "channel": {"id": channel, "type": 0, "guild_id": "G",
            "permission_overwrites": [
                {"id": "G", "type": 0, "allow": "0", "deny": "1024"}
            ]},
        "everyone": {"id": "G", "permissions": "1024"}
    }))
    .expect("fixture")
}

#[test]
fn slack_and_discord_private_channels_agree_on_viewers_and_scopes() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    const N: usize = 5;
    let members: Vec<MemberId> = (0..N).map(|i| format!("m{i}")).collect();
    let slack_ids: Vec<String> = (0..N).map(|i| format!("U{i}")).collect();
    let discord_ids: Vec<String> = (0..N).map(|i| format!("D{i}")).collect();

    let mut links = Links::new();
    for i in 0..N {
        links.link(
            Identity {
                platform: Platform::Slack,
                team: "T".into(),
                user: slack_ids[i].clone(),
            },
            members[i].clone(),
        );
        links.link(
            Identity {
                platform: Platform::Discord,
                team: "G".into(),
                user: discord_ids[i].clone(),
            },
            members[i].clone(),
        );
    }

    let slack = Slack {
        bot_user_id: "B".into(),
    };
    let discord = Discord {
        application_id: "APP".into(),
    };

    for _ in 0..1000 {
        // Membership subset S: who is in the private channels.
        let in_s: Vec<bool> = (0..N).map(|_| rng.bool()).collect();
        let s_members: BTreeSet<MemberId> = (0..N)
            .filter(|&i| in_s[i])
            .map(|i| members[i].clone())
            .collect();

        // --- Slack private channel ---
        let slack_chan = "G_SLACK";
        let slack_roster: SlackRoster = (0..N)
            .filter(|&i| in_s[i])
            .map(|i| slack_ids[i].clone())
            .collect();
        let slack_msg = slack
            .receive(&slack_event(slack_chan, &slack_ids[0]))
            .expect("a message");
        assert_eq!(
            slack_msg.location.visibility,
            Visibility::Private,
            "slack private"
        );

        // --- Discord private channel: @everyone denied, role R allowed ---
        let discord_chan = "C_DISCORD";
        let everyone = Role {
            id: "G".into(),
            permissions: VIEW_CHANNEL,
        };
        let role_r = Role {
            id: "R".into(),
            permissions: 0,
        };
        // A couple of random unrelated roles that never grant VIEW_CHANNEL or
        // ADMINISTRATOR (which would short-circuit to true for everyone).
        let noise = rng.next() & !(VIEW_CHANNEL | ADMINISTRATOR);
        let extra_roles = vec![
            Role {
                id: "X1".into(),
                permissions: noise,
            },
            Role {
                id: "X2".into(),
                permissions: !noise & !(VIEW_CHANNEL | ADMINISTRATOR),
            },
        ];
        let mut roles = vec![role_r];
        roles.extend(extra_roles.clone());
        let overwrites = vec![
            Overwrite {
                id: "G".into(),
                kind: 0,
                allow: 0,
                deny: VIEW_CHANNEL,
            },
            Overwrite {
                id: "R".into(),
                kind: 0,
                allow: VIEW_CHANNEL,
                deny: 0,
            },
        ];
        let mut d_members = Vec::new();
        for i in 0..N {
            let mut roles_vec = Vec::new();
            if in_s[i] {
                roles_vec.push("R".into());
            }
            // Sprinkle random unrelated roles.
            if rng.bool() {
                roles_vec.push("X1".into());
            }
            if rng.bool() {
                roles_vec.push("X2".into());
            }
            d_members.push(GuildMember {
                user_id: discord_ids[i].clone(),
                roles: roles_vec,
            });
        }
        let discord_roster = DiscordRoster {
            guild_id: "G".into(),
            everyone: everyone.clone(),
            roles: roles.clone(),
            overwrites: overwrites.clone(),
            members: d_members,
        };
        let discord_msg = discord
            .receive(&discord_event(discord_chan, &discord_ids[0]))
            .expect("a message");
        assert_eq!(
            discord_msg.location.visibility,
            Visibility::Private,
            "discord private"
        );

        // (2) viewers map through Links to the same member set, on both platforms.
        let slack_viewers: BTreeSet<MemberId> = slack
            .viewers(&slack_roster)
            .iter()
            .map(|uid| {
                links
                    .member(&Identity {
                        platform: Platform::Slack,
                        team: "T".into(),
                        user: uid.clone(),
                    })
                    .expect("linked")
                    .clone()
            })
            .collect();
        let discord_viewers: BTreeSet<MemberId> = discord
            .viewers(&discord_roster)
            .iter()
            .map(|uid| {
                links
                    .member(&Identity {
                        platform: Platform::Discord,
                        team: "G".into(),
                        user: uid.clone(),
                    })
                    .expect("linked")
                    .clone()
            })
            .collect();
        assert_eq!(slack_viewers, s_members, "slack viewers == S");
        assert_eq!(discord_viewers, s_members, "discord viewers == S");

        // For each member, the private channels they can view (their roster membership).
        // A member is a viewer iff in_s[member].
        for i in 0..N {
            let member = &members[i];
            let is_viewer = in_s[i];
            let mut private_channels = Vec::new();
            if is_viewer {
                private_channels.push(slack_msg.location.clone());
                private_channels.push(discord_msg.location.clone());
            }

            // (3) in-channel scopes_for: {shared, channel:<key>} for a member
            // of the channel; a non-member gets `shared` alone, because the
            // channel scope is granted on membership evidence only.
            for loc in [&slack_msg.location, &discord_msg.location] {
                let scopes = scopes_for(member, loc, &private_channels);
                let expected = if is_viewer {
                    BTreeSet::from([Scope::Shared, Scope::Channel(loc.key())])
                } else {
                    BTreeSet::from([Scope::Shared])
                };
                assert_eq!(
                    scopes, expected,
                    "in-channel scopes for member {i} on {}",
                    loc.platform
                );
            }

            // (3) in a DM: channel:<key> appears iff the member is a viewer.
            let dm = Location {
                platform: Platform::Slack,
                channel: "DM".into(),
                visibility: Visibility::Direct,
            };
            let dm_scopes = scopes_for(member, &dm, &private_channels);
            assert_eq!(
                dm_scopes.contains(&Scope::Channel(slack_msg.location.key())),
                is_viewer,
                "DM channel scope for member {i} (slack key)"
            );
            assert_eq!(
                dm_scopes.contains(&Scope::Channel(discord_msg.location.key())),
                is_viewer,
                "DM channel scope for member {i} (discord key)"
            );
            assert!(dm_scopes.contains(&Scope::Shared), "DM always has shared");
            assert!(
                dm_scopes.contains(&Scope::User(member.clone())),
                "DM has user"
            );

            // (4) scope_of for the private message is Channel of its own key.
            // The author is the member, not `msg.author.user`: a platform user
            // id ("U0"/"D0") is a different id space from a member id, and
            // passing one only compiles because `MemberId` is a `String`.
            for loc in [&slack_msg.location, &discord_msg.location] {
                let scope = scope_of(loc, &members[i]);
                assert_eq!(
                    scope,
                    Scope::Channel(loc.key()),
                    "scope_of for {}",
                    loc.platform
                );
                // ...readable via scopes_for in a DM exactly for the viewers.
                assert_eq!(
                    dm_scopes.contains(&scope),
                    is_viewer,
                    "scope readable in DM for member {i}"
                );
            }
        }
    }
}

#[test]
fn a_private_channel_scope_needs_membership() {
    let private = Location {
        platform: Platform::Slack,
        channel: "G1".into(),
        visibility: Visibility::Private,
    };
    let member = "m0".to_string();
    let member_scopes = scopes_for(&member, &private, std::slice::from_ref(&private));
    assert_eq!(
        member_scopes,
        BTreeSet::from([Scope::Shared, Scope::Channel(private.key())]),
        "a member of the private channel reads it"
    );
    let non_member_scopes = scopes_for(&member, &private, &[]);
    assert_eq!(
        non_member_scopes,
        BTreeSet::from([Scope::Shared]),
        "a non-member asking about a private channel gets shared only"
    );
}

#[test]
fn public_channels_yield_only_shared() {
    let slack = Slack {
        bot_user_id: "B".into(),
    };
    let discord = Discord {
        application_id: "APP".into(),
    };

    // Slack public channel.
    let slack_msg = slack
        .receive(
            &serde_json::from_value(json!({
                "team_id": "T", "event": {
                    "type": "message", "user": "U1", "channel": "C1",
                    "channel_type": "channel", "text": "hi", "ts": "1.1"
                }
            }))
            .expect("fixture"),
        )
        .expect("a message");
    assert_eq!(slack_msg.location.visibility, Visibility::Public);
    assert_eq!(
        scopes_for(&"m0".to_string(), &slack_msg.location, &[]),
        BTreeSet::from([Scope::Shared]),
    );

    // Discord public channel (@everyone can view, no deny overwrite).
    let discord_msg = discord
        .receive(
            &serde_json::from_value(json!({
                "message": {"id": "M", "channel_id": "C1", "guild_id": "G",
                    "author": {"id": "D1"}, "content": "hi", "mentions": []},
                "channel": {"id": "C1", "type": 0, "guild_id": "G"},
                "everyone": {"id": "G", "permissions": "1024"}
            }))
            .expect("fixture"),
        )
        .expect("a message");
    assert_eq!(discord_msg.location.visibility, Visibility::Public);
    assert_eq!(
        scopes_for(&"m0".to_string(), &discord_msg.location, &[]),
        BTreeSet::from([Scope::Shared]),
    );
}
