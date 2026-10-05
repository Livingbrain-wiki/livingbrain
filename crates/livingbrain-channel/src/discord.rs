//! The Discord adapter: receive from the gateway, and the post bodies for a
//! reply, a reaction and an approval card.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::json;

use crate::{
    ApprovalCard, Channel, Identity, Location, Message, Outbound, Platform, Reply, Visibility,
};

/// View Channels permission bit.
const VIEW_CHANNEL: u64 = 1 << 10;
/// Administrator permission bit.
const ADMINISTRATOR: u64 = 1 << 3;

/// The Discord adapter. `application_id` is the bot's application (user) id.
#[derive(Debug, Clone)]
pub struct Discord {
    pub application_id: String,
}

/// Parses Discord's decimal-string permission fields into bits.
fn permissions_from_str<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    raw.parse::<u64>().map_err(serde::de::Error::custom)
}

/// A permission overwrite for a channel (role or member).
#[derive(Debug, Deserialize, Clone)]
pub struct Overwrite {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(deserialize_with = "permissions_from_str")]
    pub allow: u64,
    #[serde(deserialize_with = "permissions_from_str")]
    pub deny: u64,
}

/// A guild role.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Role {
    pub id: String,
    #[serde(deserialize_with = "permissions_from_str")]
    pub permissions: u64,
}

/// A member of a guild.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct GuildMember {
    pub user_id: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// A Discord channel, narrowed to the fields `receive` reads. `kind`: 0 text,
/// 1 DM, 3 group DM, 11 public thread, 12 private thread.
#[derive(Debug, Deserialize)]
pub struct DiscordChannel {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub permission_overwrites: Vec<Overwrite>,
}

/// A message author.
#[derive(Debug, Deserialize)]
pub struct Author {
    pub id: String,
    #[serde(default)]
    pub bot: bool,
}

/// A user mention in a message.
#[derive(Debug, Deserialize)]
pub struct Mention {
    pub id: String,
}

/// A gateway message, narrowed to the fields `receive` reads.
#[derive(Debug, Deserialize)]
pub struct DiscordMessage {
    pub id: String,
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    pub author: Author,
    /// Set when the message came through a webhook. Such a message reports
    /// `bot: false`, so without this field the brain would read back its own
    /// (or anyone else's) webhook output and answer it.
    #[serde(default)]
    pub webhook_id: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<Mention>,
}

/// A gateway event: the message, its channel, the parent (for threads) and
/// the guild's `@everyone` role.
#[derive(Debug, Deserialize)]
pub struct DiscordEvent {
    pub message: DiscordMessage,
    pub channel: DiscordChannel,
    pub parent: Option<DiscordChannel>,
    /// Absent outside a guild — a DM has no `@everyone` role to classify.
    #[serde(default)]
    pub everyone: Role,
}

/// A guild channel's roster: the guild's roles, the channel's overwrites and
/// its members.
#[derive(Debug, Default, Clone)]
pub struct Roster {
    pub guild_id: String,
    pub everyone: Role,
    pub roles: Vec<Role>,
    pub overwrites: Vec<Overwrite>,
    pub members: Vec<GuildMember>,
}

impl Channel for Discord {
    const PLATFORM: Platform = Platform::Discord;
    type Event = DiscordEvent;
    type Roster = Roster;

    fn receive(&self, event: &Self::Event) -> Option<Message> {
        let msg = &event.message;
        if msg.author.bot || msg.webhook_id.is_some() || msg.author.id == self.application_id {
            return None;
        }
        // Threads: the location is the parent; the thread id tags the message.
        let (channel, thread, visibility) = if event.channel.kind == 11 || event.channel.kind == 12
        {
            let parent = event.parent.as_ref()?;
            // A private thread (12) inherits the parent's classification like
            // a public one (11): a thread in a DM is still a DM, and a thread in
            // a public channel is still public, whatever its own kind says.
            let vis = channel_visibility(parent, &event.everyone);
            (
                parent.id.clone(),
                Some(event.message.channel_id.clone()),
                vis,
            )
        } else {
            (
                event.channel.id.clone(),
                None,
                channel_visibility(&event.channel, &event.everyone),
            )
        };
        let mentions_bot = msg.mentions.iter().any(|m| m.id == self.application_id);
        let team = msg.guild_id.clone().unwrap_or_else(|| "@me".to_owned());
        Some(Message {
            author: Identity {
                platform: Platform::Discord,
                team,
                user: msg.author.id.clone(),
            },
            location: Location {
                platform: Platform::Discord,
                channel,
                visibility,
            },
            thread,
            id: msg.id.clone(),
            text: msg.content.clone(),
            mentions_bot,
        })
    }

    fn reply(&self, to: &Message, reply: &Reply) -> Outbound {
        let mut content = reply.text.clone();
        if !reply.citations.is_empty() {
            let cites = reply
                .citations
                .iter()
                .map(|c| {
                    format!(
                        "[{}](<{}>)",
                        escape_markdown(&c.title),
                        escape_link_url(&c.url)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            content.push('\n');
            content.push_str(&cites);
        }
        let channel = target_channel(to);
        Outbound {
            platform: Self::PLATFORM,
            method: format!("POST /channels/{channel}/messages"),
            channel: channel.clone(),
            thread: to.thread.clone(),
            body: json!({
                "content": content,
                "message_reference": {"message_id": to.id},
                "allowed_mentions": {"parse": []},
            }),
        }
    }

    fn react(&self, to: &Message, emoji: &str) -> Outbound {
        let channel = target_channel(to);
        Outbound {
            platform: Self::PLATFORM,
            method: format!(
                "PUT /channels/{channel}/messages/{}/reactions/{emoji}",
                to.id
            ),
            channel,
            thread: to.thread.clone(),
            body: json!({"emoji": emoji}),
        }
    }

    fn approval(&self, to: &Message, card: &ApprovalCard) -> Outbound {
        let channel = target_channel(to);
        Outbound {
            platform: Self::PLATFORM,
            method: format!("POST /channels/{channel}/messages"),
            channel,
            thread: to.thread.clone(),
            body: json!({
                "content": card.prompt,
                "components": [{
                    "type": 1,
                    "components": [
                        {"type": 2, "style": 3, "label": "Approve",
                         "custom_id": format!("approve:{}", card.id)},
                        {"type": 2, "style": 4, "label": "Deny",
                         "custom_id": format!("deny:{}", card.id)},
                    ],
                }],
            }),
        }
    }

    fn viewers(&self, roster: &Self::Roster) -> BTreeSet<String> {
        roster
            .members
            .iter()
            .filter(|m| {
                can_view(
                    &roster.everyone,
                    &roster.roles,
                    &roster.overwrites,
                    &roster.guild_id,
                    m,
                )
            })
            .map(|m| m.user_id.clone())
            .collect()
    }
}

/// Whether `member` can view a channel, per Discord's documented algorithm.
fn can_view(
    everyone: &Role,
    roles: &[Role],
    channel_overwrites: &[Overwrite],
    guild_id: &str,
    member: &GuildMember,
) -> bool {
    let mut base = everyone.permissions;
    for role in roles {
        if member.roles.contains(&role.id) {
            base |= role.permissions;
        }
    }
    if base & ADMINISTRATOR != 0 {
        return true;
    }
    let mut perms = base;
    // @everyone overwrite.
    for ow in channel_overwrites {
        if ow.id == guild_id {
            perms &= !ow.deny;
            perms |= ow.allow;
        }
    }
    // Member-role overwrites, deny union then allow union.
    let (mut role_deny, mut role_allow) = (0u64, 0u64);
    for ow in channel_overwrites {
        if ow.kind == 0 && member.roles.contains(&ow.id) {
            role_deny |= ow.deny;
            role_allow |= ow.allow;
        }
    }
    perms &= !role_deny;
    perms |= role_allow;
    // Member-specific overwrite.
    for ow in channel_overwrites {
        if ow.kind == 1 && ow.id == member.user_id {
            perms &= !ow.deny;
            perms |= ow.allow;
        }
    }
    perms & VIEW_CHANNEL != 0
}

/// The channel a reply, reaction or approval goes to: a message inside a
/// thread is posted back into that thread, not into the parent channel.
fn target_channel(to: &Message) -> String {
    to.thread
        .clone()
        .unwrap_or_else(|| to.location.channel.clone())
}

/// Visibility of a (non-thread) channel: DM/group by kind, else role-gated iff
/// `@everyone` alone cannot view it.
fn channel_visibility(channel: &DiscordChannel, everyone: &Role) -> Visibility {
    match &channel.guild_id {
        None => match channel.kind {
            1 => Visibility::Direct,
            3 => Visibility::Private,
            // Fail closed: a kind we do not know (forum, announcement, stage,
            // media — anything new) must not be filed under `shared`, where
            // anyone in the venture could read it.
            _ => Visibility::Private,
        },
        Some(guild_id) => {
            let everyone_member = GuildMember {
                user_id: String::new(),
                roles: vec![],
            };
            if can_view(
                everyone,
                &[],
                &channel.permission_overwrites,
                guild_id,
                &everyone_member,
            ) {
                Visibility::Public
            } else {
                Visibility::Private
            }
        }
    }
}

/// Escapes `[` and `]` so a title cannot break out of a markdown link.
fn escape_markdown(s: &str) -> String {
    s.replace('[', "\\[").replace(']', "\\]")
}

/// Escapes `<` and `>` so a url cannot close the `<...>` slot of a markdown
/// link. The url comes from model output and stored sources, so it is as
/// untrusted as the title.
fn escape_link_url(s: &str) -> String {
    s.replace('<', "\\<").replace('>', "\\>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Citation, Visibility};
    use serde_json::Value;

    fn app() -> Discord {
        Discord {
            application_id: "APP".into(),
        }
    }

    /// A guild text-channel event from `author`, with overwrites JSON spliced in.
    fn guild_ev(overwrites: Value, author: Value) -> DiscordEvent {
        serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "C1", "guild_id": "G",
                "author": author, "content": "hi", "mentions": []},
            "channel": {"id": "C1", "type": 0, "guild_id": "G",
                "permission_overwrites": overwrites},
            "everyone": {"id": "G", "permissions": "1024"}
        }))
        .expect("fixture")
    }

    #[test]
    fn receives_a_public_guild_message() {
        let got = app()
            .receive(&guild_ev(json!([]), json!({"id": "D1"})))
            .expect("a message");
        assert_eq!(got.location.visibility, Visibility::Public);
        assert_eq!(got.author.user, "D1");
        assert!(!got.mentions_bot);
    }

    #[test]
    fn receives_a_role_gated_private_channel() {
        let ow = json!([{"id": "G", "type": 0, "allow": "0", "deny": "1024"}]);
        let got = app()
            .receive(&guild_ev(ow, json!({"id": "D1"})))
            .expect("a message");
        assert_eq!(got.location.visibility, Visibility::Private);
    }

    #[test]
    fn receives_a_dm() {
        // No `everyone`: a DM has no guild role to classify.
        let ev: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "C1", "author": {"id": "D1"}, "content": "hi"},
            "channel": {"id": "C1", "type": 1}
        }))
        .expect("fixture");
        let got = app().receive(&ev).expect("a message");
        assert_eq!(got.location.visibility, Visibility::Direct);
        assert_eq!(got.author.team, "@me");
    }

    #[test]
    fn an_unknown_non_guild_channel_kind_is_not_public() {
        let ev: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "F1", "author": {"id": "D1"}, "content": "hi"},
            "channel": {"id": "F1", "type": 15}
        }))
        .expect("fixture");
        let got = app().receive(&ev).expect("a message");
        assert_ne!(got.location.visibility, Visibility::Public);
        assert_eq!(got.location.visibility, Visibility::Private);
    }

    #[test]
    fn ignores_bot_webhook_and_self_messages() {
        let bot = guild_ev(json!([]), json!({"id": "D1", "bot": true}));
        let self_msg = guild_ev(json!([]), json!({"id": "APP"}));
        let webhook: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "C1", "guild_id": "G",
                "author": {"id": "D1"}, "webhook_id": "W1", "content": "hi", "mentions": []},
            "channel": {"id": "C1", "type": 0, "guild_id": "G"},
            "everyone": {"id": "G", "permissions": "1024"}
        }))
        .expect("fixture");
        assert!(app().receive(&bot).is_none());
        assert!(app().receive(&self_msg).is_none());
        assert!(app().receive(&webhook).is_none());
    }

    #[test]
    fn mention_flags_mention() {
        let ev: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "C1", "guild_id": "G",
                "author": {"id": "D1"}, "content": "hi", "mentions": [{"id": "APP"}]},
            "channel": {"id": "C1", "type": 0, "guild_id": "G"},
            "everyone": {"id": "G", "permissions": "1024"}
        }))
        .expect("fixture");
        assert!(app().receive(&ev).expect("a message").mentions_bot);
    }

    #[test]
    fn can_view_everyone_denied_role_allowed() {
        let everyone = Role {
            id: "G".into(),
            permissions: VIEW_CHANNEL,
        };
        let roles = [Role {
            id: "R".into(),
            permissions: 0,
        }];
        let ow = vec![
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
        let member_in = GuildMember {
            user_id: "D1".into(),
            roles: vec!["R".into()],
        };
        let member_out = GuildMember {
            user_id: "D2".into(),
            roles: vec![],
        };
        assert!(can_view(&everyone, &roles, &ow, "G", &member_in));
        assert!(!can_view(&everyone, &roles, &ow, "G", &member_out));
    }

    #[test]
    fn can_view_member_overwrite_grants() {
        let everyone = Role {
            id: "G".into(),
            permissions: 0,
        };
        let ow = vec![
            Overwrite {
                id: "G".into(),
                kind: 0,
                allow: 0,
                deny: VIEW_CHANNEL,
            },
            Overwrite {
                id: "D1".into(),
                kind: 1,
                allow: VIEW_CHANNEL,
                deny: 0,
            },
        ];
        let member = GuildMember {
            user_id: "D1".into(),
            roles: vec![],
        };
        assert!(can_view(&everyone, &[], &ow, "G", &member));
    }

    #[test]
    fn can_view_administrator_short_circuits() {
        let everyone = Role {
            id: "G".into(),
            permissions: ADMINISTRATOR,
        };
        let ow = vec![Overwrite {
            id: "G".into(),
            kind: 0,
            allow: 0,
            deny: VIEW_CHANNEL,
        }];
        let member = GuildMember {
            user_id: "D1".into(),
            roles: vec![],
        };
        assert!(can_view(&everyone, &[], &ow, "G", &member));
    }

    #[test]
    fn thread_inherits_parent_visibility() {
        let parent_gated = json!([{"id": "G", "type": 0, "allow": "0", "deny": "1024"}]);
        let ev: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "T1", "guild_id": "G",
                "author": {"id": "D1"}, "content": "hi", "mentions": []},
            "channel": {"id": "T1", "type": 11, "guild_id": "G"},
            "parent": {"id": "C1", "type": 0, "guild_id": "G",
                "permission_overwrites": parent_gated},
            "everyone": {"id": "G", "permissions": "1024"}
        }))
        .expect("fixture");
        let got = app().receive(&ev).expect("a message");
        assert_eq!(got.location.channel, "C1");
        assert_eq!(got.thread.as_deref(), Some("T1"));
        assert_eq!(got.location.visibility, Visibility::Private);
        assert_eq!(got.conversation_key(), "discord:G:C1:T1");
    }

    #[test]
    fn a_private_thread_in_a_dm_is_still_direct() {
        let ev: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "T1",
                "author": {"id": "D1"}, "content": "hi", "mentions": []},
            "channel": {"id": "T1", "type": 12},
            "parent": {"id": "C1", "type": 1}
        }))
        .expect("fixture");
        let got = app().receive(&ev).expect("a message");
        assert_eq!(got.location.channel, "C1");
        assert_eq!(got.location.visibility, Visibility::Direct);
    }

    #[test]
    fn reply_targets_its_channel_and_carries_citations() {
        let to = app()
            .receive(&guild_ev(json!([]), json!({"id": "D1"})))
            .unwrap();
        let out = app().reply(
            &to,
            &Reply {
                text: "answer".into(),
                citations: vec![Citation {
                    title: "[x]".into(),
                    url: "https://x".into(),
                }],
            },
        );
        assert_eq!(out.platform, <Discord as Channel>::PLATFORM);
        assert_eq!(out.method, "POST /channels/C1/messages");
        assert_eq!(out.channel, "C1");
        assert_eq!(out.thread, None);
        assert_eq!(out.body["message_reference"]["message_id"], "M1");
        assert!(
            out.body["content"]
                .as_str()
                .unwrap()
                .contains("[\\[x\\]](<https://x>)")
        );
    }

    #[test]
    fn citation_url_cannot_break_out_of_the_link() {
        let to = app()
            .receive(&guild_ev(json!([]), json!({"id": "D1"})))
            .unwrap();
        let out = app().reply(
            &to,
            &Reply {
                text: "answer".into(),
                citations: vec![Citation {
                    title: "t".into(),
                    url: "https://x/a>)[click](https://evil".into(),
                }],
            },
        );
        // The url's own `>` would otherwise close the `<...>` target early and
        // let the rest of it render as a link of its own.
        assert!(
            out.body["content"]
                .as_str()
                .unwrap()
                .contains("[t](<https://x/a\\>)[click](https://evil>)")
        );
    }

    #[test]
    fn react_targets_the_message_and_the_channel_it_sits_in() {
        let to = app()
            .receive(&guild_ev(json!([]), json!({"id": "D1"})))
            .unwrap();
        let out = app().react(&to, "eyes");
        assert_eq!(out.platform, <Discord as Channel>::PLATFORM);
        assert_eq!(out.method, "PUT /channels/C1/messages/M1/reactions/eyes");
        assert_eq!(out.channel, "C1");
        assert_eq!(out.body["emoji"], "eyes");
    }

    #[test]
    fn a_reply_into_a_thread_goes_to_the_thread() {
        let parent_gated = json!([{"id": "G", "type": 0, "allow": "0", "deny": "1024"}]);
        let ev: DiscordEvent = serde_json::from_value(json!({
            "message": {"id": "M1", "channel_id": "T1", "guild_id": "G",
                "author": {"id": "D1"}, "content": "hi", "mentions": []},
            "channel": {"id": "T1", "type": 11, "guild_id": "G"},
            "parent": {"id": "C1", "type": 0, "guild_id": "G",
                "permission_overwrites": parent_gated},
            "everyone": {"id": "G", "permissions": "1024"}
        }))
        .expect("fixture");
        let to = app().receive(&ev).expect("a message");
        let out = app().reply(
            &to,
            &Reply {
                text: "answer".into(),
                citations: vec![],
            },
        );
        assert_eq!(out.channel, "T1");
        assert_eq!(out.method, "POST /channels/T1/messages");
        assert_eq!(out.thread.as_deref(), Some("T1"));
    }

    #[test]
    fn approval_body_has_approve_and_deny_components() {
        let to = app()
            .receive(&guild_ev(json!([]), json!({"id": "D1"})))
            .unwrap();
        let out = app().approval(
            &to,
            &ApprovalCard {
                id: "42".into(),
                prompt: "ok?".into(),
            },
        );
        assert_eq!(out.platform, <Discord as Channel>::PLATFORM);
        assert_eq!(out.method, "POST /channels/C1/messages");
        assert_eq!(out.channel, "C1");
        let comps = &out.body["components"][0]["components"];
        assert_eq!(comps[0]["custom_id"], "approve:42");
        assert_eq!(comps[1]["custom_id"], "deny:42");
        assert_eq!(comps[1]["style"], 4);
    }
}
