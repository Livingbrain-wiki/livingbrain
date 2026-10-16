//! The pure half of the import: which messages a transcript keeps, how a
//! channel-day is named and bounded, and how one day becomes one Markdown
//! source. No I/O here; the drain feeds it what Discord returned.
//!
//! A source is one channel on one UTC day. Its body lists each kept message
//! in time order: the time, the author's display name (`(bot)` for a bot),
//! the reply it answers when Discord says so, the text, and the attachments
//! as filename and URL. Only the page-store pipeline redacts it, as it does
//! every import.

use crate::api::{KEPT_TYPES, Message, User};

/// Discord's epoch, in unix milliseconds: 2015-01-01T00:00:00Z.
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
const DAY_MS: i64 = 86_400_000;

/// Whether a message belongs in a transcript: a plain message, a reply or a
/// slash command. System messages are not kept.
#[must_use]
pub fn keeps(message: &Message) -> bool {
    KEPT_TYPES.contains(&message.kind)
}

/// The UTC day a Discord timestamp falls on, `YYYY-MM-DD`, or `None` when it
/// is not the RFC 3339 shape Discord sends.
#[must_use]
pub fn day_of(timestamp: &str) -> Option<String> {
    let head = timestamp.get(..10)?;
    let well_formed = head.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    (well_formed && timestamp.as_bytes().get(10) == Some(&b'T')).then(|| head.to_owned())
}

/// `HH:MM:SS` of a timestamp, or `??:??:??`.
fn time_of(timestamp: &str) -> String {
    match timestamp.get(11..19) {
        Some(clock)
            if clock.bytes().enumerate().all(|(i, b)| {
                if i == 2 || i == 5 {
                    b == b':'
                } else {
                    b.is_ascii_digit()
                }
            }) =>
        {
            clock.to_owned()
        }
        _ => "??:??:??".to_owned(),
    }
}

/// The first and last snowflake a UTC day can hold, as `(start, end)`: a
/// message belongs to the day when its id lies in `[start, end)`. `None` for a
/// malformed day.
#[must_use]
pub fn day_bounds(day: &str) -> Option<(u64, u64)> {
    let mut parts = day.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let date_of_month: u8 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = time::Month::try_from(month).ok()?;
    let date = time::Date::from_calendar_date(year, month, date_of_month).ok()?;
    let start_ms = date
        .with_time(time::Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp()
        .checked_mul(1000)?;
    Some((
        snowflake_at(start_ms - DISCORD_EPOCH_MS)?,
        snowflake_at(start_ms + DAY_MS - DISCORD_EPOCH_MS)?,
    ))
}

/// The smallest snowflake created at `ms` after Discord's epoch.
fn snowflake_at(ms_since_epoch: i64) -> Option<u64> {
    let ms = u64::try_from(ms_since_epoch).ok()?;
    ms.checked_mul(1 << 22)
}

/// The Markdown `rel_path` a channel-day is filed under.
#[must_use]
pub fn rel_path(guild_id: &str, channel_id: &str, day: &str) -> String {
    format!("discord/{guild_id}/{channel_id}/{day}")
}

/// The permalink of a channel, the source's `origin_ref`.
#[must_use]
pub fn origin_ref(guild_id: &str, channel_id: &str) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}")
}

/// A user as a reader sees them: the display name, then the username, with
/// `(bot)` after a bot.
#[must_use]
pub fn author_label(user: &User) -> String {
    let name = user
        .global_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&user.username);
    let name = if name.trim().is_empty() {
        "unknown"
    } else {
        name
    };
    if user.bot == Some(true) {
        format!("{name} (bot)")
    } else {
        name.to_owned()
    }
}

/// The message ids as numbers, for ordering (Discord ids are snowflakes, so
/// the numeric order is the time order).
#[must_use]
pub fn id_number(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

/// One channel-day as Markdown. `messages` may hold any day and any kind: only
/// the kept messages of `day` are written, in time order. Answers `None` when
/// none are left.
#[must_use]
pub fn transcript(
    guild_name: &str,
    channel_name: &str,
    day: &str,
    messages: &[Message],
) -> Option<String> {
    let mut kept: Vec<&Message> = messages
        .iter()
        .filter(|m| keeps(m) && day_of(&m.timestamp).as_deref() == Some(day))
        .collect();
    if kept.is_empty() {
        return None;
    }
    kept.sort_by_key(|m| id_number(&m.id));
    kept.dedup_by_key(|m| m.id.clone());
    let mut out = String::new();
    let noun = if kept.len() == 1 {
        "message"
    } else {
        "messages"
    };
    out.push_str(&format!(
        "# #{channel_name} in {guild_name}\n\nUTC day {day} · {} {noun}\n",
        kept.len()
    ));
    for message in kept {
        out.push('\n');
        let mut heading = format!(
            "**{} · {}**",
            time_of(&message.timestamp),
            author_label(&message.author)
        );
        if message.message_reference.is_some() {
            match &message.referenced_message {
                Some(replied) => {
                    heading.push_str(&format!(" · reply to {}", author_label(&replied.author)))
                }
                None => heading.push_str(" · reply"),
            }
        }
        out.push_str(&heading);
        out.push('\n');
        let text = message.content.trim();
        out.push_str(if text.is_empty() { "(no text)" } else { text });
        out.push('\n');
        for attachment in &message.attachments {
            out.push_str(&format!(
                "[attachment: {} · {}]\n",
                attachment.filename, attachment.url
            ));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Attachment, Referenced};

    fn user(name: &str, bot: Option<bool>) -> User {
        User {
            username: name.to_owned(),
            global_name: None,
            bot,
        }
    }

    fn msg(id: &str, timestamp: &str, kind: u8, text: &str) -> Message {
        Message {
            id: id.to_owned(),
            kind,
            timestamp: timestamp.to_owned(),
            content: text.to_owned(),
            author: user("alice", None),
            attachments: Vec::new(),
            message_reference: None,
            referenced_message: None,
        }
    }

    #[test]
    fn the_utc_day_comes_from_the_timestamp() {
        assert_eq!(
            day_of("2026-10-03T23:59:59.000000+00:00").as_deref(),
            Some("2026-10-03")
        );
        assert_eq!(day_of("2026-10-03 23:59"), None);
        assert_eq!(day_of("nope"), None);
    }

    #[test]
    fn a_day_is_bounded_by_snowflakes_of_its_own_midnights() {
        let (start, end) = day_bounds("2026-10-03").expect("a day");
        assert!(start < end);
        assert_eq!(end - start, (DAY_MS as u64) << 22);
        // 2026-10-03T00:00:00Z in unix seconds is 1790985600.
        assert_eq!(
            start,
            ((1_790_985_600_i64 * 1000 - DISCORD_EPOCH_MS) as u64) << 22
        );
        assert!(day_bounds("2026-13-01").is_none());
        assert!(day_bounds("2026-10").is_none());
    }

    #[test]
    fn only_the_kept_messages_of_the_day_are_written_in_time_order() {
        let day = "2026-10-03";
        let messages = vec![
            msg("300", "2026-10-03T10:00:00+00:00", 0, "second"),
            msg("200", "2026-10-03T09:00:00+00:00", 0, "first"),
            msg("400", "2026-10-03T11:00:00+00:00", 7, "pinned notice"),
            msg("500", "2026-10-04T00:00:01+00:00", 0, "next day"),
        ];
        let body = transcript("Guild", "general", day, &messages).expect("a transcript");
        let first = body.find("first").expect("first");
        let second = body.find("second").expect("second");
        assert!(first < second, "{body}");
        assert!(!body.contains("pinned notice"), "{body}");
        assert!(!body.contains("next day"), "{body}");
        assert!(body.contains("UTC day 2026-10-03 · 2 messages"), "{body}");
        assert!(transcript("Guild", "general", "2026-10-05", &messages).is_none());
    }

    #[test]
    fn bots_replies_and_attachments_are_marked() {
        let mut reply = msg("10", "2026-10-03T08:00:00+00:00", 19, "");
        reply.author = user("bob", Some(true));
        reply.message_reference = Some(serde::de::IgnoredAny);
        reply.referenced_message = Some(Referenced {
            author: User {
                username: "carol".to_owned(),
                global_name: Some("Carol Q".to_owned()),
                bot: None,
            },
        });
        reply.attachments = vec![Attachment {
            filename: "plan.pdf".to_owned(),
            url: "https://cdn.example/plan.pdf".to_owned(),
        }];
        let body = transcript("G", "c", "2026-10-03", &[reply]).unwrap();
        assert!(
            body.contains("**08:00:00 · bob (bot)** · reply to Carol Q"),
            "{body}"
        );
        assert!(body.contains("(no text)"), "{body}");
        assert!(
            body.contains("[attachment: plan.pdf · https://cdn.example/plan.pdf]"),
            "{body}"
        );
    }

    #[test]
    fn a_duplicate_message_is_written_once() {
        let m = msg("1", "2026-10-03T08:00:00+00:00", 0, "once");
        let body = transcript("G", "c", "2026-10-03", &[m.clone(), m]).unwrap();
        assert_eq!(body.matches("once").count(), 1, "{body}");
    }
}
