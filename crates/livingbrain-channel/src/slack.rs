//! The Slack adapter: receive from the Events API, and the post bodies for a
//! reply, a reaction and an approval card.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::json;

use crate::{
    ApprovalCard, Channel, Identity, Location, Message, Outbound, Platform, Reply, Visibility,
};

/// The Slack adapter. `bot_user_id` is the app's own Slack user id, used to
/// drop the app's own messages so it never answers itself.
#[derive(Debug, Clone)]
pub struct Slack {
    pub bot_user_id: String,
}

/// An Events API envelope, narrowed to the fields [`Slack::receive`] reads.
#[derive(Debug, Deserialize)]
pub struct SlackEvent {
    pub team_id: String,
    pub event: SlackMessage,
}

/// A Slack `message` or `app_mention` event.
#[derive(Debug, Deserialize)]
pub struct SlackMessage {
    #[serde(rename = "type")]
    pub kind: String,
    pub subtype: Option<String>,
    pub bot_id: Option<String>,
    pub user: Option<String>,
    pub channel: String,
    pub channel_type: Option<String>,
    #[serde(default)]
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
}

/// A Slack conversation's roster: the member list from `conversations.members`.
pub type Roster = Vec<String>;

impl Channel for Slack {
    const PLATFORM: Platform = Platform::Slack;
    type Event = SlackEvent;
    type Roster = Roster;

    fn receive(&self, event: &Self::Event) -> Option<Message> {
        let msg = &event.event;
        if msg.kind != "message" && msg.kind != "app_mention" {
            return None;
        }
        // Edits, joins, bot traffic — anything but a human's plain text.
        if msg.subtype.is_some() || msg.bot_id.is_some() {
            return None;
        }
        let user = msg.user.as_deref()?;
        if user == self.bot_user_id {
            return None;
        }
        let visibility = visibility(msg);
        let mentions_bot =
            msg.kind == "app_mention" || msg.text.contains(&format!("<@{}>", self.bot_user_id));
        Some(Message {
            author: Identity {
                platform: Platform::Slack,
                team: event.team_id.clone(),
                user: user.to_owned(),
            },
            location: Location {
                platform: Platform::Slack,
                channel: msg.channel.clone(),
                visibility,
            },
            thread: msg.thread_ts.clone(),
            id: msg.ts.clone(),
            text: msg.text.clone(),
            mentions_bot,
        })
    }

    fn reply(&self, to: &Message, reply: &Reply) -> Outbound {
        let mut blocks = vec![json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": reply.text},
        })];
        if !reply.citations.is_empty() {
            let elements = reply
                .citations
                .iter()
                .map(|c| json!({"type": "mrkdwn", "text": format!("<{}|{}>", escape_link_url(&c.url), escape_mrkdwn(&c.title))}))
                .collect::<Vec<_>>();
            blocks.push(json!({"type": "context", "elements": elements}));
        }
        // A reply always lands in a thread: the message's own, or a thread it
        // starts — so an answer never floats away from what it answers.
        let thread = to.thread.clone().unwrap_or_else(|| to.id.clone());
        Outbound {
            platform: Self::PLATFORM,
            method: "chat.postMessage".into(),
            channel: to.location.channel.clone(),
            thread: Some(thread.clone()),
            body: json!({
                "channel": to.location.channel,
                "thread_ts": thread,
                "text": reply.text,
                "blocks": blocks,
            }),
        }
    }

    fn react(&self, to: &Message, emoji: &str) -> Outbound {
        Outbound {
            platform: Self::PLATFORM,
            method: "reactions.add".into(),
            channel: to.location.channel.clone(),
            // A reaction targets a message, not a thread.
            thread: None,
            body: json!({
                "channel": to.location.channel,
                "timestamp": to.id,
                "name": emoji,
            }),
        }
    }

    fn approval(&self, to: &Message, card: &ApprovalCard) -> Outbound {
        let thread = to.thread.clone().unwrap_or_else(|| to.id.clone());
        Outbound {
            platform: Self::PLATFORM,
            method: "chat.postMessage".into(),
            channel: to.location.channel.clone(),
            thread: Some(thread.clone()),
            body: json!({
                "channel": to.location.channel,
                "thread_ts": thread,
                "text": card.prompt,
                "blocks": [
                    {"type": "section", "text": {"type": "mrkdwn", "text": card.prompt}},
                    {"type": "actions", "elements": [
                        {"type": "button", "style": "primary",
                         "text": {"type": "plain_text", "text": "Approve"},
                         "action_id": format!("approve:{}", card.id)},
                        {"type": "button", "style": "danger",
                         "text": {"type": "plain_text", "text": "Deny"},
                         "action_id": format!("deny:{}", card.id)},
                    ]},
                ],
            }),
        }
    }

    fn viewers(&self, roster: &Self::Roster) -> BTreeSet<String> {
        roster.iter().cloned().collect()
    }
}

/// Visibility from `channel_type`; for `app_mention` (which carries none) it
/// is inferred from the channel id prefix.
fn visibility(msg: &SlackMessage) -> Visibility {
    match msg.channel_type.as_deref() {
        Some("channel") => Visibility::Public,
        Some("group") | Some("mpim") => Visibility::Private,
        Some("im") => Visibility::Direct,
        _ => match msg.channel.chars().next() {
            Some('C') => Visibility::Public,
            Some('G') => Visibility::Private,
            Some('D') => Visibility::Direct,
            // Fail closed: an unrecognised id must not be filed under
            // `shared`, where anyone in the venture could read it.
            _ => Visibility::Private,
        },
    }
}

/// Escapes `&`, `<`, `>` per Slack mrkdwn so a title cannot inject markup.
fn escape_mrkdwn(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes `&`, `<`, `>` so a url cannot break out of `<url|label>`. The url
/// comes from model output and stored sources, so it is as untrusted as the
/// title.
fn escape_link_url(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Citation, Visibility};
    use serde_json::Value;

    fn slack() -> Slack {
        Slack {
            bot_user_id: "B".into(),
        }
    }

    fn ev(json: Value) -> SlackEvent {
        serde_json::from_value(json).expect("fixture deserialises")
    }

    /// A plain message event in `channel` with `channel_type` from `user`.
    fn msg(channel: &str, channel_type: &str, user: &str) -> SlackEvent {
        ev(json!({"team_id": "T", "event": {
            "type": "message", "user": user, "channel": channel,
            "channel_type": channel_type, "text": "x", "ts": "1.1"
        }}))
    }

    #[test]
    fn receives_a_public_message() {
        let got = slack()
            .receive(&msg("C1", "channel", "U1"))
            .expect("a message");
        assert_eq!(got.location.visibility, Visibility::Public);
        assert_eq!(got.author.user, "U1");
        assert!(!got.mentions_bot);
        assert_eq!(got.conversation_key(), "slack:T:C1:1.1");
    }

    #[test]
    fn receives_a_private_group_message() {
        let got = slack()
            .receive(&msg("G1", "group", "U1"))
            .expect("a message");
        assert_eq!(got.location.visibility, Visibility::Private);
        assert_eq!(got.location.key(), "slack:G1");
    }

    #[test]
    fn receives_a_dm() {
        let got = slack().receive(&msg("D1", "im", "U1")).expect("a message");
        assert_eq!(got.location.visibility, Visibility::Direct);
    }

    #[test]
    fn app_mention_infers_visibility_from_prefix_and_flags_mention() {
        let m = ev(json!({"team_id": "T", "event": {
            "type": "app_mention", "user": "U1",
            "channel": "C9", "text": "<@B> hi", "ts": "1.1"
        }}));
        let got = slack().receive(&m).expect("a message");
        assert_eq!(got.location.visibility, Visibility::Public);
        assert!(got.mentions_bot);
    }

    #[test]
    fn app_mention_in_a_private_channel_prefix_g() {
        let m = ev(json!({"team_id": "T", "event": {
            "type": "app_mention", "user": "U1",
            "channel": "G9", "text": "<@B>", "ts": "1.1"
        }}));
        let got = slack().receive(&m).expect("a message");
        assert_eq!(got.location.visibility, Visibility::Private);
    }

    #[test]
    fn thread_parent_key_uses_thread_ts() {
        let m = ev(json!({"team_id": "T", "event": {
            "type": "message", "user": "U1", "channel": "C1",
            "channel_type": "channel", "text": "reply", "ts": "2.2", "thread_ts": "1.1"
        }}));
        let got = slack().receive(&m).expect("a message");
        assert_eq!(got.thread.as_deref(), Some("1.1"));
        assert_eq!(got.conversation_key(), "slack:T:C1:1.1");
    }

    #[test]
    fn ignores_bot_subtyped_and_self_messages() {
        let bot = ev(json!({"team_id": "T", "event": {
            "type": "message", "bot_id": "B1", "channel": "C1", "ts": "1"
        }}));
        let subtyped = ev(json!({"team_id": "T", "event": {
            "type": "message", "subtype": "message_changed", "channel": "C1", "ts": "1"
        }}));
        let self_msg = ev(json!({"team_id": "T", "event": {
            "type": "message", "user": "B", "channel": "C1", "ts": "1"
        }}));
        let no_user = ev(json!({"team_id": "T", "event": {
            "type": "message", "channel": "C1", "ts": "1"
        }}));
        let s = slack();
        assert!(s.receive(&bot).is_none());
        assert!(s.receive(&subtyped).is_none());
        assert!(s.receive(&self_msg).is_none());
        assert!(s.receive(&no_user).is_none());
    }

    #[test]
    fn unknown_channel_prefix_is_not_public() {
        let m = ev(json!({"team_id": "T", "event": {
            "type": "app_mention", "user": "U1",
            "channel": "X9", "text": "<@B>", "ts": "1.1"
        }}));
        let got = slack().receive(&m).expect("a message");
        assert_ne!(got.location.visibility, Visibility::Public);
        assert_eq!(got.location.visibility, Visibility::Private);
    }

    #[test]
    fn reply_body_has_thread_and_citation_context() {
        let to = slack().receive(&msg("C1", "channel", "U1")).unwrap();
        let out = slack().reply(
            &to,
            &Reply {
                text: "answer".into(),
                citations: vec![Citation {
                    title: "A & B <c>".into(),
                    url: "https://x".into(),
                }],
            },
        );
        assert_eq!(out.platform, <Slack as Channel>::PLATFORM);
        assert_eq!(out.method, "chat.postMessage");
        assert_eq!(out.channel, "C1");
        assert_eq!(out.thread.as_deref(), Some("1.1"));
        assert_eq!(out.body["channel"], "C1");
        assert_eq!(out.body["thread_ts"], "1.1");
        assert_eq!(out.body["blocks"][1]["type"], "context");
        assert_eq!(
            out.body["blocks"][1]["elements"][0]["text"],
            "<https://x|A &amp; B &lt;c&gt;>"
        );
    }

    #[test]
    fn citation_url_cannot_break_out_of_the_link() {
        let to = slack().receive(&msg("C1", "channel", "U1")).unwrap();
        let out = slack().reply(
            &to,
            &Reply {
                text: "answer".into(),
                citations: vec![Citation {
                    title: "t".into(),
                    url: "https://x/a>|<@evil> ".into(),
                }],
            },
        );
        // The url's own `>` would otherwise close the link target early and
        // let the rest of it read as a label.
        assert_eq!(
            out.body["blocks"][1]["elements"][0]["text"],
            "<https://x/a&gt;|&lt;@evil&gt; |t>"
        );
    }

    #[test]
    fn react_targets_the_message_it_is_given() {
        let to = slack().receive(&msg("C1", "channel", "U1")).unwrap();
        let out = slack().react(&to, "+1");
        assert_eq!(out.platform, <Slack as Channel>::PLATFORM);
        assert_eq!(out.method, "reactions.add");
        assert_eq!(out.channel, "C1");
        assert_eq!(out.thread, None);
        assert_eq!(out.body["timestamp"], "1.1");
        assert_eq!(out.body["name"], "+1");
    }

    #[test]
    fn approval_body_has_approve_and_deny_buttons() {
        let to = slack().receive(&msg("C1", "channel", "U1")).unwrap();
        let out = slack().approval(
            &to,
            &ApprovalCard {
                id: "42".into(),
                prompt: "ok?".into(),
            },
        );
        assert_eq!(out.platform, <Slack as Channel>::PLATFORM);
        assert_eq!(out.method, "chat.postMessage");
        assert_eq!(out.channel, "C1");
        assert_eq!(out.thread.as_deref(), Some("1.1"));
        let actions = &out.body["blocks"][1]["elements"];
        assert_eq!(actions[0]["action_id"], "approve:42");
        assert_eq!(actions[1]["action_id"], "deny:42");
        assert_eq!(actions[0]["style"], "primary");
        assert_eq!(actions[1]["style"], "danger");
    }

    #[test]
    fn viewers_are_the_roster_set() {
        let got = slack().viewers(&vec!["U1".into(), "U2".into(), "U1".into()]);
        assert_eq!(got, BTreeSet::from(["U1".into(), "U2".into()]));
    }
}
