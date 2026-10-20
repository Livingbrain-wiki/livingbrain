//! A `Database` that enforces D1's bound-parameter ceiling, for tests.
//!
//! Two shapes. [`BindLimited`] wraps a real database (the SQLite one the
//! harness builds) and refuses any statement over the ceiling before it
//! reaches the engine, so the real SQL runs and the real answers come back,
//! and the only difference from D1 is the ceiling. [`D1Like`] answers every
//! query with no rows, for a test that only needs the statement to be
//! accepted.
//!
//! A module's tests wrap the database they were given:
//!
//! ```ignore
//! let db = BindLimited::new(kit.db.clone());
//! let store = SourceStore::new(db.clone(), ...);
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use cratefield_core::{Database, DbError, Rows, Statement};

pub use livingbrain_d1::D1_MAX_BINDS;

/// Refuses `stmt` when it binds more than [`D1_MAX_BINDS`] values, with the
/// message D1 itself gives.
fn refuse_if_over(stmt: &Statement) -> Result<usize, String> {
    let binds = stmt.values.0.len();
    if binds > D1_MAX_BINDS {
        return Err(format!(
            "too many SQL variables: {binds} bound parameters (D1 allows {D1_MAX_BINDS})"
        ));
    }
    Ok(binds)
}

/// A real database behind D1's bind ceiling. Every statement is checked
/// before it runs; the widest statement seen is kept for assertions.
pub struct BindLimited {
    inner: Arc<dyn Database>,
    widest: AtomicUsize,
}

impl BindLimited {
    /// Wraps `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Database>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            widest: AtomicUsize::new(0),
        })
    }

    /// The most binds any statement this database saw carried.
    #[must_use]
    pub fn widest(&self) -> usize {
        self.widest.load(Ordering::SeqCst)
    }

    fn check(&self, stmt: &Statement) -> Result<(), String> {
        let binds = refuse_if_over(stmt)?;
        self.widest.fetch_max(binds, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Database for BindLimited {
    async fn execute(&self, stmt: &Statement) -> Result<u64, DbError> {
        self.check(stmt).map_err(DbError::Execute)?;
        self.inner.execute(stmt).await
    }

    async fn query(&self, stmt: &Statement) -> Result<Rows, DbError> {
        self.check(stmt).map_err(DbError::Query)?;
        self.inner.query(stmt).await
    }

    async fn batch_atomic(&self, stmts: &[Statement]) -> Result<(), DbError> {
        for stmt in stmts {
            self.check(stmt).map_err(DbError::Batch)?;
        }
        self.inner.batch_atomic(stmts).await
    }
}

/// Answers every query with no rows, and refuses an over-ceiling statement.
#[derive(Default)]
pub struct D1Like;

#[async_trait]
impl Database for D1Like {
    async fn execute(&self, stmt: &Statement) -> Result<u64, DbError> {
        refuse_if_over(stmt).map(|_| 0).map_err(DbError::Execute)
    }

    async fn query(&self, stmt: &Statement) -> Result<Rows, DbError> {
        refuse_if_over(stmt).map_err(DbError::Query)?;
        Ok(Rows::new(Vec::new()))
    }

    async fn batch_atomic(&self, stmts: &[Statement]) -> Result<(), DbError> {
        for stmt in stmts {
            refuse_if_over(stmt).map_err(DbError::Batch)?;
        }
        Ok(())
    }
}
