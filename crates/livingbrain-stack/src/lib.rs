//! Living Brain's "Built with" list: the `uses` array of venture FZ-018 out of
//! the published Factory Zero stack registry.
//!
//! `stack.json` beside this file is a **vendored copy** of one entry of
//! <https://factory0.ventures/stack.json>, cut for this repo. The registry is
//! the source of truth and this crate holds a copy of it; there is deliberately
//! **no runtime fetch**. The Worker renders on a request and the CLI renders on
//! a cold terminal, and neither may depend on a third-party host being up, so
//! there is no HTTP client in this crate's dependency graph and no code path
//! that can fail open on a timeout.
//!
//! When an entry changes the change lands in the registry **first** and then
//! here: re-vendor `stack.json`, re-check it against the registry with
//! `scripts/check-vendored-stack.sh`, and update the pinned SHA-256 in
//! `tests/stack.rs`.
//!
//! # Status is an enum, not a string
//!
//! The one thing this list must never do is present a decided-but-unbuilt
//! thing as if it were running. So `status` is a [`Status`], the renderers
//! take that enum and get their word from [`Status::as_str`], and there is no
//! path by which a hand-typed "live" reaches a line of output.
//!
//! # A plain library
//!
//! No ports, no tables, no routes: data plus the two renderers, the same
//! shape as `livingbrain-redact`.
#![forbid(unsafe_code)]

use std::fmt::Write;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Column widths for [`Stack::text`], chosen so the widest row
/// (`Hosted on` / `Cloudflare` / `https://www.cloudflare.com`) lands at 68
/// columns — inside the usual 72-column terminal.
const PHRASE_COLUMNS: usize = 18;
const NAME_COLUMNS: usize = 16;
const STATUS_COLUMNS: usize = 9;

/// The venture this list describes, and where it is published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Venture {
    /// The registry's own identifier for the venture (`FZ-018`).
    pub id: String,
    /// The venture's name.
    pub name: String,
    /// Where the venture itself lives.
    pub site: String,
    /// The venture's page in the registry, which carries the rest of the
    /// published list.
    pub page: String,
}

/// The one sentence the registry spells for each status, in words rather than
/// as a bare adjective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Statuses {
    /// What `live` means here.
    pub live: String,
    /// What `planned` means here.
    pub planned: String,
}

impl Statuses {
    /// The sentence for `status`, taken from the key rather than matched by
    /// hand, so a status the file does not carry cannot be rendered at all.
    pub fn get(&self, status: Status) -> &str {
        match status {
            Status::Live => &self.live,
            Status::Planned => &self.planned,
        }
    }
}

/// Whether an entry is in use today or only decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// In use today.
    Live,
    /// Decided and tracked, not in use yet.
    Planned,
}

impl Status {
    /// Both statuses, in the order the legend prints them.
    pub const ALL: [Self; 2] = [Self::Live, Self::Planned];

    /// The registry's word for this status. The only source of the word; no
    /// renderer spells it out.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Planned => "planned",
        }
    }

    /// Whether this entry is in use today.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }

    /// The registry's own sentence for this status.
    #[must_use]
    pub fn gloss(self, statuses: &Statuses) -> &str {
        statuses.get(self)
    }
}

/// One line of the "Built with" list: a role, the phrase that introduces it,
/// the thing doing it, and what that means today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UseEntry {
    /// The role's slug (`framework`, `hosting`, …), stable across renames.
    pub role: String,
    /// The words the line introduces the product with ("Built with").
    pub phrase: String,
    /// The product's name.
    pub name: String,
    /// The product's registry id, or its vendor's slug for a third party.
    pub id: String,
    /// `factory-zero`, or `third-party` for something outside the registry.
    pub kind: String,
    /// Where the product lives.
    pub url: String,
    /// Whether it is in use today.
    pub status: Status,
    /// Why it is on the list, in the registry's words.
    pub note: String,
}

impl UseEntry {
    /// Whether this entry is in use today.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.status.is_live()
    }
}

/// The vendored registry entry, as this crate serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stack {
    /// The venture the list is about.
    pub venture: Venture,
    /// What each status means, in words.
    pub statuses: Statuses,
    /// The entries, in the registry's order.
    pub uses: Vec<UseEntry>,
    /// Where this file was copied from.
    pub source: String,
    /// The venture's subprocessors list.
    pub subprocessors: String,
}

impl Stack {
    /// Parse a vendored stack document. The same shape as the venture's
    /// `MailTheme::from_json`, so a malformed edit fails loudly here rather
    /// than at the first render.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The entries, in the registry's order.
    #[must_use]
    pub fn entries(&self) -> &[UseEntry] {
        &self.uses
    }

    /// The entries that are in use today, in the registry's order.
    #[must_use]
    pub fn live(&self) -> Vec<&UseEntry> {
        self.uses.iter().filter(|entry| entry.is_live()).collect()
    }

    /// The list as plain text: what `livingbrain about` prints.
    ///
    /// Every status word comes from [`Status::as_str`], and each row is
    /// checked against its own entry's status before the whole thing is
    /// returned, so a renderer that ever swapped the two words fails here
    /// rather than on someone's screen.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{} — Built with", self.venture.name);
        let _ = writeln!(
            out,
            "The Factory Zero stack registry entry {}. Nothing here is a promise of a date.",
            self.venture.id
        );
        let _ = writeln!(out);
        out.push_str(&self.rows());
        let _ = writeln!(out);
        for status in Status::ALL {
            let _ = writeln!(
                out,
                "{} {}",
                pad(status.as_str(), PHRASE_COLUMNS),
                status.gloss(&self.statuses)
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "Subprocessors: {}", self.subprocessors);
        let _ = writeln!(
            out,
            "              listed on the venture page; no privacy page yet"
        );
        let _ = writeln!(out, "Registry:     {}", self.source);
        debug_assert!(
            self.rows_hold_their_own_status(),
            "a rendered row shows a status its entry does not have"
        );
        out
    }

    /// The list as a Markdown bullet list, the same content shape the PWA and
    /// the bot render, so there is one set of facts and one set of words.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "## Built with");
        let _ = writeln!(out);
        for entry in &self.uses {
            let _ = writeln!(
                out,
                "- **{}** [{}]({}) — *{}*: {}",
                entry.phrase,
                entry.name,
                entry.url,
                entry.status.as_str(),
                entry.note
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "Subprocessors: <{}>", self.subprocessors);
        let _ = writeln!(out, "Listed on the venture page; no privacy page yet.");
        let _ = writeln!(out, "Registry: <{}>", self.source);
        out
    }

    /// Whether every rendered row carries its own entry's status and not the
    /// other one. The invariant the whole crate exists to hold, checked on
    /// the rendered text rather than on the data.
    fn rows_hold_their_own_status(&self) -> bool {
        let rows = self.rows();
        self.uses.iter().all(|entry| {
            let Some(row) = rows
                .lines()
                .find(|line| line.contains(&entry.name) && line.contains(&entry.url))
            else {
                return false;
            };
            let other = Status::ALL
                .into_iter()
                .find(|status| *status != entry.status)
                .expect("the two statuses differ");
            row.contains(&format!(" {} ", entry.status.as_str()))
                && !row.contains(&format!(" {} ", other.as_str()))
        })
    }

    /// The entries as aligned rows — the one place a status word is written
    /// into a row, so `text` and the invariant check cannot drift apart.
    fn rows(&self) -> String {
        let mut out = String::new();
        for entry in &self.uses {
            let _ = writeln!(
                out,
                "{} {} {} {}",
                pad(&entry.phrase, PHRASE_COLUMNS),
                pad(&entry.name, NAME_COLUMNS),
                pad(entry.status.as_str(), STATUS_COLUMNS),
                entry.url
            );
        }
        out
    }
}

/// `value` padded to `columns`, or `value` followed by two spaces when it is
/// longer — a column that cannot be padded is never truncated, because a
/// product name cut in half is a wrong answer rather than a tidy one.
fn pad(value: &str, columns: usize) -> String {
    let width = value.chars().count();
    if width >= columns {
        format!("{value}  ")
    } else {
        format!("{value}{}", " ".repeat(columns - width))
    }
}

static STACK: OnceLock<Stack> = OnceLock::new();

/// The vendored stack entry, parsed once.
///
/// The parse is a runtime one — the file is read into the binary by
/// `include_str!` and deserialized on the first call — so a malformed edit
/// fails here, with the file named, rather than at the first render.
#[must_use]
pub fn stack() -> &'static Stack {
    STACK.get_or_init(|| {
        Stack::from_json(include_str!("stack.json"))
            .expect("stack.json is a valid Stack: re-vendor it from the registry")
    })
}

/// The vendored file's own text, byte for byte. What the drift guard hashes,
/// and what a re-vendor step diffs.
#[must_use]
pub fn source_text() -> &'static str {
    include_str!("stack.json")
}
