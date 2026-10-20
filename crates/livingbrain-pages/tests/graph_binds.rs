//! The brain map's reads stay under D1's bound-parameter limit.
//!
//! Cloudflare D1 refuses any statement with more than 100 bound parameters
//! ("too many SQL variables"), and a member whose grants name many projects
//! or scopes makes `graph_rows_in` bind one parameter per name. The
//! production Worker then answers the map with a 500. The fake below enforces
//! the same ceiling, so a regression fails here rather than in production.

use livingbrain_d1_fake::D1Like;
use livingbrain_pages::access::ScopeFilter;
use livingbrain_pages::{GraphReach, graph_rows_in};

fn names(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}-{i:03}")).collect()
}

#[pollster::test]
async fn a_member_with_many_granted_projects_is_read_within_the_bind_limit() {
    // Colony sync files each repository's sessions under its own project, so
    // a grant over the workspace's repositories names many projects at once.
    let reach = GraphReach {
        pages: Vec::new(),
        sources: Vec::new(),
        source_projects: vec![("shared-abc".to_owned(), names("repo", 120))],
        facts: Vec::new(),
        fact_subjects: Vec::new(),
    };
    let db = D1Like;
    let result = graph_rows_in(&db, "T0WORKSPACE", &reach, &|_, _| true, 800).await;
    assert!(result.is_ok(), "the map read failed: {:?}", result.err());
}

#[pollster::test]
async fn a_reach_over_many_scopes_is_read_within_the_bind_limit() {
    let reach = GraphReach {
        pages: names("scope", 120)
            .into_iter()
            .map(|scope| ScopeFilter::whole(scope, "shared"))
            .collect(),
        sources: names("scope", 120),
        source_projects: Vec::new(),
        facts: names("fact-scope", 120),
        fact_subjects: Vec::new(),
    };
    let db = D1Like;
    let result = graph_rows_in(&db, "T0WORKSPACE", &reach, &|_, _| true, 800).await;
    assert!(result.is_ok(), "the map read failed: {:?}", result.err());
}
