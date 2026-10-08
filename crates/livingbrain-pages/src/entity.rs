//! Entity types, typed frontmatter and `[[wiki-link]]` extraction.
//!
//! The body may open with a small frontmatter block: `---` on its own line,
//! then flat `key: value` lines, then `---` again. Which keys are required,
//! optional or an enum is decided by [`EntityType`], not the caller: the same
//! body is valid as a Person page and invalid as a Project one.

use std::collections::{BTreeMap, BTreeSet};

use crate::store::PageError;

/// The kinds of thing a page can describe. The set is closed on purpose: a
/// new entity type is a schema change, not a string a caller can invent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityType {
    Person,
    Project,
    Decision,
    Customer,
    System,
    Glossary,
    /// Something published elsewhere that this workspace watches: a paper,
    /// a release note, a security advisory (issue #58). Its own type
    /// rather than a glossary term, because a radar page is not a
    /// definition and its frontmatter is a different shape.
    Radar,
}

impl EntityType {
    /// Every variant, so a caller can enumerate them without a `match`.
    pub const ALL: [EntityType; 7] = [
        EntityType::Person,
        EntityType::Project,
        EntityType::Decision,
        EntityType::Customer,
        EntityType::System,
        EntityType::Glossary,
        EntityType::Radar,
    ];

    /// The stable wire name, also what a `type:` frontmatter key holds.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Project => "project",
            Self::Decision => "decision",
            Self::Customer => "customer",
            Self::System => "system",
            Self::Glossary => "glossary",
            Self::Radar => "radar",
        }
    }

    /// Parses a wire name; `None` for anything else.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "person" => Some(Self::Person),
            "project" => Some(Self::Project),
            "decision" => Some(Self::Decision),
            "customer" => Some(Self::Customer),
            "system" => Some(Self::System),
            "glossary" => Some(Self::Glossary),
            "radar" => Some(Self::Radar),
            _ => None,
        }
    }
}

/// The longest a slug (or a scope) may be.
pub const MAX_SLUG_LEN: usize = 128;

/// The slug rule: lowercase ascii letters, digits and `-`, 1..=128 chars.
/// Scopes follow the same rule.
#[must_use]
pub fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SLUG_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A page body's parsed frontmatter: flat `key: value` pairs, keys lowercased.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    /// The parsed pairs, keyed by lowercased key. Includes `type` when present.
    pub fields: BTreeMap<String, String>,
}

impl Frontmatter {
    /// The value for `key`, if the frontmatter carried one.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

/// The keys one entity type accepts. `required` must be present and non-empty;
/// `optional` may be absent; every key in `enums` must hold one of its values.
struct Schema {
    required: &'static [&'static str],
    optional: &'static [&'static str],
    enums: &'static [(&'static str, &'static [&'static str])],
}

fn schema(entity: EntityType) -> Schema {
    match entity {
        EntityType::Person => Schema {
            required: &["name"],
            optional: &["role", "email"],
            enums: &[],
        },
        EntityType::Project => Schema {
            required: &["name"],
            optional: &["status", "owner"],
            enums: &[("status", &["active", "paused", "done"])],
        },
        EntityType::Decision => Schema {
            required: &["title"],
            optional: &["status", "date"],
            enums: &[("status", &["proposed", "accepted", "superseded"])],
        },
        EntityType::Customer => Schema {
            required: &["name"],
            optional: &["stage", "owner"],
            enums: &[],
        },
        EntityType::System => Schema {
            required: &["name"],
            optional: &["owner"],
            enums: &[],
        },
        EntityType::Glossary => Schema {
            required: &["term"],
            optional: &["aliases"],
            enums: &[],
        },
        // A radar page names what was published and where it came from
        // (issue #58). `score` is the judge's number and `urgent` says the
        // item matched a locked dependency, so a page can be triaged by
        // reading its frontmatter.
        EntityType::Radar => Schema {
            required: &["title"],
            optional: &["source", "url", "topic", "score", "urgent"],
            enums: &[("urgent", &["yes", "no"])],
        },
    }
}

/// Splits a leading frontmatter block from the Markdown that follows: the
/// block between the `---` fences and the body after them, or `None` when the
/// body opens without a fence or the block is never closed.
fn split_frontmatter(markdown: &str) -> Option<(&str, &str)> {
    let mut lines = markdown.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return None;
    }
    let mut offset = first.len();
    let mut block_end = None;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            block_end = Some(offset);
            offset += line.len();
            break;
        }
        offset += line.len();
    }
    let block_end = block_end?;
    Some((&markdown[first.len()..block_end], &markdown[offset..]))
}

/// Parses and validates a page body against its entity type, returning the
/// frontmatter and the Markdown that follows it (the body links are read from).
///
/// # Errors
///
/// [`PageError::Frontmatter`] for a missing required key, an unknown key, a
/// bad enum value or an unparseable `type:`, and [`PageError::TypeMismatch`]
/// when a `type:` key disagrees with `entity`.
pub fn parse_frontmatter(
    markdown: &str,
    entity: EntityType,
) -> Result<(Frontmatter, String), PageError> {
    let Some((block, body)) = split_frontmatter(markdown) else {
        let frontmatter = Frontmatter::default();
        validate(&frontmatter, entity)?;
        return Ok((frontmatter, markdown.to_owned()));
    };

    let mut fields = BTreeMap::new();
    for (index, raw) in block.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(PageError::Frontmatter(format!(
                "line {}: expected `key: value`, found `{line}`",
                index + 2
            )));
        };
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(PageError::Frontmatter(format!(
                "line {}: the key is empty",
                index + 2
            )));
        }
        fields.insert(key, value.trim().trim_matches('"').to_owned());
    }

    let frontmatter = Frontmatter { fields };
    validate(&frontmatter, entity)?;
    Ok((frontmatter, body.to_owned()))
}

fn validate(frontmatter: &Frontmatter, entity: EntityType) -> Result<(), PageError> {
    let schema = schema(entity);

    if let Some(declared) = frontmatter.get("type") {
        let Some(found) = EntityType::parse(declared) else {
            return Err(PageError::Frontmatter(format!(
                "unknown entity type `{declared}`"
            )));
        };
        if found != entity {
            return Err(PageError::TypeMismatch {
                declared: entity,
                found,
            });
        }
    }

    for key in frontmatter.fields.keys() {
        if key == "type" {
            continue;
        }
        if !schema.required.contains(&key.as_str()) && !schema.optional.contains(&key.as_str()) {
            return Err(PageError::Frontmatter(format!(
                "unknown key `{key}` for a {} page",
                entity.as_str()
            )));
        }
    }

    for key in schema.required {
        match frontmatter.get(key) {
            Some(value) if !value.trim().is_empty() => {}
            _ => {
                return Err(PageError::Frontmatter(format!(
                    "a {} page requires `{key}`",
                    entity.as_str()
                )));
            }
        }
    }

    for (key, allowed) in schema.enums {
        if let Some(value) = frontmatter.get(key)
            && !allowed.contains(&value)
        {
            return Err(PageError::Frontmatter(format!(
                "`{key}` must be one of {}, found `{value}`",
                allowed.join(", ")
            )));
        }
    }

    Ok(())
}

/// Extracts the targets of `[[slug]]` and `[[slug|label]]` links from a body.
///
/// Targets are lowercased, deduplicated and returned sorted, and each must be
/// a valid slug: a link to something that cannot be a page is an error rather
/// than a silently dropped edge.
///
/// # Errors
///
/// [`PageError::InvalidLink`] when a target is not a slug.
pub fn extract_links(body: &str) -> Result<Vec<String>, PageError> {
    let mut links: BTreeSet<String> = BTreeSet::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        let target = inner
            .split_once('|')
            .map_or(inner, |(target, _)| target)
            .trim();
        let normalised = target.to_ascii_lowercase();
        if !is_slug(&normalised) {
            return Err(PageError::InvalidLink(target.to_owned()));
        }
        links.insert(normalised);
        rest = &after[end + 2..];
    }
    Ok(links.into_iter().collect())
}
