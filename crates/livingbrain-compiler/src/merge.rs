//! The merge two compiles need when they wrote the same page at once.
//!
//! Compiles run in parallel ([`crate::compile`]), and many sessions of one
//! project write the same project pages (`decisions`, `index`, the rollup).
//! The model answers with a whole page body based on the version it was
//! shown; when another compile wrote the page in the meantime, writing that
//! body would drop the other compile's lines. Instead the compiler merges:
//!
//! - **base**: the body the model was shown (empty for a page that did not
//!   exist yet);
//! - **theirs**: the page as it is now, with the other compile's lines;
//! - **ours**: the body the model answered.
//!
//! The merge is line-wise and append-friendly, which is how these pages
//! grow (dated bullets under a few headings): lines ours removed from base
//! are removed from theirs, and lines ours added are inserted after the
//! nearest line before them that the result still has — so a bullet lands
//! under its own heading — unless theirs already has the very same line.
//! Lines theirs added stay. Nothing is ever dropped silently: a line both
//! sides rewrote differently ends up twice, once as each wrote it, which a
//! later compile (or a person) tidies.

use std::collections::BTreeMap;

/// `ours` replayed onto `theirs` over their common `base`.
#[must_use]
pub fn three_way(base: &str, theirs: &str, ours: &str) -> String {
    let base_lines: Vec<&str> = base.lines().collect();
    let ours_lines: Vec<&str> = ours.lines().collect();
    let mut result: Vec<String> = theirs.lines().map(str::to_owned).collect();

    // What ours removed from base (by count, blank lines aside): taken out
    // of theirs, once per removal, where theirs still has it.
    let mut kept = counts(&ours_lines);
    let mut removed: Vec<&str> = Vec::new();
    for line in &base_lines {
        if line.trim().is_empty() {
            continue;
        }
        match kept.get_mut(line) {
            Some(n) if *n > 0 => *n -= 1,
            _ => removed.push(line),
        }
    }
    for line in removed {
        if let Some(at) = result.iter().position(|have| have == line) {
            result.remove(at);
        }
    }

    // What ours added to base (by count): each line ours has more often than
    // base did. Blank lines are never "added" on their own; a heading gets a
    // blank line before it when it is inserted.
    let mut in_base = counts(&base_lines);
    let mut added = vec![false; ours_lines.len()];
    for (index, line) in ours_lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match in_base.get_mut(line) {
            Some(n) if *n > 0 => *n -= 1,
            _ => added[index] = true,
        }
    }

    // Where each line of ours sits in the result, matched in order; an
    // added line goes right after the nearest earlier line that has a place.
    let mut cursor = 0usize;
    let mut last: Option<usize> = None;
    for (index, line) in ours_lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if added[index] {
            if let Some(found) = result.iter().position(|have| have == line) {
                // Theirs already says it: no duplicate.
                last = Some(found);
                cursor = cursor.max(found + 1);
                continue;
            }
            let mut at = last.map_or(0, |anchor| anchor + 1);
            if line.starts_with('#') {
                // A new section goes after the whole section it follows, so
                // the lines theirs added there stay under their heading.
                while at < result.len() && !result[at].starts_with('#') {
                    at += 1;
                }
                while at > 0
                    && last.is_some_and(|anchor| at > anchor + 1)
                    && result[at - 1].trim().is_empty()
                {
                    at -= 1;
                }
                if at > 0 && !result[at - 1].trim().is_empty() {
                    result.insert(at, String::new());
                    at += 1;
                }
                result.insert(at, (*line).to_owned());
                result.insert(at + 1, String::new());
                // What follows the heading goes after its blank line.
                last = Some(at + 1);
                cursor = at + 2;
                continue;
            }
            // Under a heading, after the blank line that follows it.
            if last.is_some_and(|anchor| result[anchor].starts_with('#'))
                && result.get(at).is_some_and(|next| next.trim().is_empty())
            {
                at += 1;
            }
            result.insert(at, (*line).to_owned());
            last = Some(at);
            cursor = at + 1;
            continue;
        }
        // A line of ours that base had: find it in the result, in order.
        let found = result
            .iter()
            .skip(cursor)
            .position(|have| have == line)
            .map(|offset| cursor + offset)
            .or_else(|| result.iter().position(|have| have == line));
        if let Some(found) = found {
            last = Some(found);
            cursor = found + 1;
        }
    }

    let mut out = result.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn counts<'a>(lines: &[&'a str]) -> BTreeMap<&'a str, usize> {
    let mut out = BTreeMap::new();
    for line in lines {
        if !line.trim().is_empty() {
            *out.entry(*line).or_default() += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_bullets_land_under_their_heading() {
        let base = "# Decisions\n\n## 2026-10\n\n- a\n";
        let theirs = "# Decisions\n\n## 2026-10\n\n- a\n- theirs\n";
        let ours = "# Decisions\n\n## 2026-10\n\n- a\n- ours\n";
        let merged = three_way(base, theirs, ours);
        assert_eq!(
            merged,
            "# Decisions\n\n## 2026-10\n\n- a\n- ours\n- theirs\n"
        );
    }

    #[test]
    fn a_new_page_written_twice_keeps_both_bodies() {
        let theirs = "# Decisions\n\n- theirs\n\n## Sources\n\n- [one](x)\n";
        let ours = "# Decisions\n\n- ours\n\n## Sources\n\n- [two](y)\n";
        let merged = three_way("", theirs, ours);
        for line in ["- theirs", "- ours", "- [one](x)", "- [two](y)"] {
            assert!(merged.contains(line), "{line} lost:\n{merged}");
        }
        assert_eq!(merged.matches("## Sources").count(), 1, "{merged}");
        assert_eq!(merged.matches("# Decisions").count(), 1, "{merged}");
        // Our source sits under the one Sources heading.
        let sources = merged.find("## Sources").expect("the heading");
        assert!(merged.find("- [two](y)").expect("ours") > sources);
    }

    #[test]
    fn ours_removals_and_rewrites_apply_to_theirs() {
        let base = "- keep\n- drop\n- old wording\n";
        let theirs = "- keep\n- drop\n- old wording\n- theirs\n";
        let ours = "- keep\n- new wording\n";
        let merged = three_way(base, theirs, ours);
        assert_eq!(merged, "- keep\n- new wording\n- theirs\n");
    }

    #[test]
    fn a_new_section_goes_after_the_section_it_follows() {
        let base = "# T\n\n- a\n";
        let theirs = "# T\n\n- a\n- b\n";
        let ours = "# T\n\n- a\n\n## New\n\n- n\n";
        let merged = three_way(base, theirs, ours);
        assert_eq!(merged, "# T\n\n- a\n- b\n\n## New\n\n- n\n");
    }

    #[test]
    fn nothing_changed_on_our_side_is_theirs() {
        let base = "- a\n";
        let theirs = "- a\n- b\n";
        assert_eq!(three_way(base, theirs, base), "- a\n- b\n");
    }
}
