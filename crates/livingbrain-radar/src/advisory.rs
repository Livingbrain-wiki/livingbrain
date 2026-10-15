//! Security advisories, and the lockfile that says whether they matter.
//!
//! An advisory is an item like any other — screened, paged, proposable —
//! with two differences. It **skips the relevance judge**, because a
//! workspace that has locked a vulnerable version is always relevant to it,
//! and it is **urgent** when a locked package *and* version fall inside its
//! vulnerable range. An advisory for a package this workspace does not
//! depend on, or at a version it did not lock, is filtered like anything
//! else.
//!
//! GitHub's advisory API spells a range as a comma-separated conjunction —
//! `">= 1.0.0, < 1.2.3"`. That grammar is small and closed, so it is parsed
//! here rather than through `semver`: the crate would be a new dependency in
//! a Worker binary for four operators, and the version comparison underneath
//! is ten lines. A comparator this module does not understand **never
//! matches** rather than matching everything — an unreadable advisory must
//! not be able to make every dependency look urgent.

use std::cmp::Ordering;

/// One advisory, in the fields the radar needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    pub id: String,
    pub ecosystem: Ecosystem,
    pub package: String,
    pub vulnerable: String,
    pub url: String,
    pub title: String,
    pub summary: String,
}

impl Advisory {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        ecosystem: Ecosystem,
        package: impl Into<String>,
        vulnerable: impl Into<String>,
        url: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            ecosystem,
            package: package.into(),
            vulnerable: vulnerable.into(),
            url: url.into(),
            title: title.into(),
            summary: summary.into(),
        }
    }

    #[must_use]
    pub fn affects(&self, version: &str) -> bool {
        let (Some(range), Some(version)) = (
            VersionRange::parse(&self.vulnerable),
            Version::parse(version),
        ) else {
            return false;
        };
        range.contains(version)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ecosystem {
    Cargo,
    Npm,
}

impl Ecosystem {
    /// The stable wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
        }
    }
}

/// One package a workspace has locked, at the version it locked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dependency {
    pub ecosystem: Ecosystem,
    pub name: String,
    pub version: String,
}

impl Dependency {
    #[must_use]
    pub fn new(ecosystem: Ecosystem, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            ecosystem,
            name: name.into(),
            version: version.into(),
        }
    }
}

/// Reads a `Cargo.lock`'s `[[package]]` blocks: every `name` and `version`
/// pair, in file order.
///
/// Hand-scanned rather than parsed with `toml`: the file has one block
/// shape this cares about, and a TOML parser is a dependency in a Worker
/// binary for it. A block with no `version` (the root package, which names
/// itself without one) is skipped rather than locked at an empty version.
#[must_use]
pub fn parse_cargo_lock(text: &str) -> Vec<Dependency> {
    let mut packages = Vec::new();
    for block in text.split("[[package]]").skip(1) {
        // Stop at the next top-level table, which is where a package block
        // ends: a `[[bin]]` or `[metadata]` after it is not part of it.
        let block = block.split("\n[").next().unwrap_or(block);
        let (Some(name), Some(version)) = (field(block, "name"), field(block, "version")) else {
            continue;
        };
        packages.push(Dependency::new(Ecosystem::Cargo, name, version));
    }
    packages
}

fn field(block: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = \"");
    block.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(&prefix)?;
        rest.split('"').next().map(str::to_owned)
    })
}

/// A semantic version, as far as an advisory's range grammar needs: three
/// numbers and whether the release was a pre-release.
///
/// A version it cannot parse is `None`, and a range containing one never
/// matches — an unreadable version must not make everything vulnerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Whether the version carried a pre-release tail (`1.2.3-alpha.1`).
    /// Kept rather than dropped: a pre-release sorts *below* the release
    /// with the same three numbers, so `>= 1.2.3` does not match
    /// `1.2.3-alpha.1` while `< 1.2.3` does — the difference between a
    /// workspace that ships the release and one on the nightly.
    pub pre: bool,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            // A pre-release is *less than* its release: `false` before
            // `true`, so an equal triple puts the release last.
            .then_with(|| other.pre.cmp(&self.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Version {
    /// Parses `major[.minor[.patch]]`, padding what is missing with zero —
    /// `"< 0.5"` and `">= 1.0"` are how advisories are actually written —
    /// and tolerating a leading `v` and a trailing pre-release or build
    /// tail (`1.2.3-alpha.1+build`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        // A build tail is not a pre-release — `1.2.3+build` *is* `1.2.3` —
        // so it is dropped before the pre-release marker is looked for.
        let text = text.split_once('+').map_or(text, |(core, _)| core);
        let (core, pre) = match text.split_once('-') {
            Some((core, _)) => (core, true),
            None => (text, false),
        };
        let mut parts = core.split('.');
        // A missing component is a zero, but an unreadable one is not a
        // version: `1.x.3` is `None`, not `1.0.3`.
        let mut next = || match parts.next() {
            None => Some(0),
            Some(part) => part.parse::<u64>().ok(),
        };
        let version = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
            pre,
        };
        // A fourth component is not a version this understands.
        parts.next().is_none().then_some(version)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Comparator {
    op: Op,
    bound: Version,
}

/// The five operators a range may use. `=` is also the bare form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
}

/// A vulnerable range: a comma-separated conjunction, all of which must
/// hold. `None` from [`VersionRange::parse`] means the range could not be
/// read, and [`VersionRange::contains`] is then never reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange {
    comparators: Vec<Comparator>,
}

impl VersionRange {
    /// Parses `">= 1.0.0, < 1.2.3"`. `None` when any comparator is not one
    /// of the five operators with a version it can read.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut comparators = Vec::new();
        for part in text.split(',') {
            if part.trim().is_empty() {
                continue;
            }
            comparators.push(parse_comparator(part.trim())?);
        }
        (!comparators.is_empty()).then_some(Self { comparators })
    }

    /// Whether `version` satisfies every comparator.
    #[must_use]
    pub fn contains(&self, version: Version) -> bool {
        self.comparators.iter().all(|c| match c.op {
            Op::Ge => version >= c.bound,
            Op::Gt => version > c.bound,
            Op::Le => version <= c.bound,
            Op::Lt => version < c.bound,
            Op::Eq => version == c.bound,
        })
    }
}

fn parse_comparator(text: &str) -> Option<Comparator> {
    // `>=` before `>`, `<=` before `<`, so the longer operator wins.
    let (op, rest) = [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
    ]
    .into_iter()
    .find_map(|(prefix, op)| Some((op, text.strip_prefix(prefix)?)))
    .unwrap_or((Op::Eq, text));
    Some(Comparator {
        op,
        bound: Version::parse(rest)?,
    })
}
