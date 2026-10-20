//! The brain map's reads stay under D1's bound-parameter limit.
//!
//! Cloudflare D1 refuses any statement with more than 100 bound parameters
//! ("too many SQL variables"), and a member whose grants name many projects
//! or scopes makes `graph_rows_in` bind one parameter per name. The
//! production Worker then answers the map with a 500. The fake below enforces
//! the same ceiling, so a regression fails here rather than in production.

use std::sync::Mutex;

use async_trait::async_trait;
use cratefield_core::{Database, DbError, Rows, Statement};
use livingbrain_pages::access::ScopeFilter;
use livingbrain_pages::{GraphReach, graph_rows_in};

/// D1's documented ceiling on bound parameters per statement.
const D1_MAX_BINDS: usize = 100;

#[derive(Default)]
struct D1Like {
    statements: Mutex<usize>,
}

#[async_trait]
impl Database for D1Like {
    async fn execute(&self, _stmt: &Statement) -> Result<u64, DbError> {
        Ok(0)
    }

    async fn query(&self, stmt: &Statement) -> Result<Rows, DbError> {
        *self.statements.lock().unwrap() += 1;
        let binds = stmt.values.iter().count();
        if binds > D1_MAX_BINDS {
            return Err(DbError::Query(format!(
                "too many SQL variables: {binds} bound parameters (D1 allows {D1_MAX_BINDS})"
            )));
        }
        Ok(Rows::new(Vec::new()))
    }

    async fn batch_atomic(&self, _stmts: &[Statement]) -> Result<(), DbError> {
        Ok(())
    }
}

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
    let db = D1Like::default();
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
    let db = D1Like::default();
    let result = graph_rows_in(&db, "T0WORKSPACE", &reach, &|_, _| true, 800).await;
    assert!(result.is_ok(), "the map read failed: {:?}", result.err());
}
