//! Cloudflare D1 refuses any statement that binds more than 100 parameters
//! ("too many SQL variables"), and SQLite, which the tests run on, does not.
//! A statement whose bind count grows with its input (an `IN (…)` list, a
//! predicate with one clause per granted prefix, a multi-row insert) therefore
//! passes every in-process test and fails in production.
//!
//! The rule every such statement follows: keep each statement's binds within
//! [`BIND_BUDGET`] by splitting the input into groups, run one statement per
//! group, and merge the answers exactly as one statement would have ordered,
//! de-duplicated and capped them. [`pack`] and [`distinct_chunks`] are the
//! splitting; the merge is the caller's, because it depends on the query.
//!
//! `livingbrain-d1-fake` is the test double that enforces the ceiling.

use std::collections::BTreeSet;

/// D1's documented ceiling on bound parameters per statement.
pub const D1_MAX_BINDS: usize = 100;

/// The binds one statement may carry for its variable part. The rest of the
/// ceiling is left for the statement's fixed binds (a workspace, a row limit,
/// a cursor): a group's budget is `BIND_BUDGET` minus those.
pub const BIND_BUDGET: usize = 90;

/// `n` positional placeholders, comma-separated: `?, ?, ?`.
#[must_use]
pub fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// Packs `items` into consecutive groups whose costs each stay within
/// `budget`. An item over budget on its own gets a group of its own, so a
/// caller that cannot split such an item still gets it through, and the
/// statement it builds fails loudly under the fake rather than silently.
pub fn pack<T>(items: Vec<T>, budget: usize, cost: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut used = 0;
    for item in items {
        let weight = cost(&item);
        if !current.is_empty() && used + weight > budget {
            groups.push(std::mem::take(&mut current));
            used = 0;
        }
        used += weight;
        current.push(item);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// The distinct values of `items`, in sorted order, in groups of at most
/// `size`. Distinct first, because a value that appears in two groups would
/// be counted twice by a `GROUP BY` answer merged across them.
#[must_use]
pub fn distinct_chunks<T: Ord + Clone>(items: &[T], size: usize) -> Vec<Vec<T>> {
    let distinct: BTreeSet<&T> = items.iter().collect();
    let all: Vec<T> = distinct.into_iter().cloned().collect();
    all.chunks(size.max(1)).map(<[T]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_comma_separated() {
        assert_eq!(placeholders(0), "");
        assert_eq!(placeholders(1), "?");
        assert_eq!(placeholders(3), "?, ?, ?");
    }

    #[test]
    fn pack_keeps_each_group_within_budget() {
        let groups = pack((0..10).collect(), 4, |_| 1);
        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|group| group.len() <= 4));
        let flat: Vec<i32> = groups.into_iter().flatten().collect();
        assert_eq!(flat, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn pack_gives_an_oversized_item_its_own_group() {
        let groups = pack(vec![1, 9, 1], 4, |n| *n);
        assert_eq!(groups, vec![vec![1], vec![9], vec![1]]);
    }

    #[test]
    fn distinct_chunks_drop_repeats_and_respect_the_size() {
        let chunks = distinct_chunks(&["b", "a", "b", "c", "a"], 2);
        assert_eq!(chunks, vec![vec!["a", "b"], vec!["c"]]);
        assert!(distinct_chunks::<&str>(&[], 2).is_empty());
    }
}
