//! A [`Fact`]: one thing that was said, in the words it was said in.

use livingbrain_access::Scope;

/// What kind of claim a fact is.
///
/// The kind is kept, not interpreted. `World` is what is true about the
/// outside — a person's employer, a deploy's version; `Experience` is what
/// happened — what was decided, by whom, when. Nothing in this crate branches
/// on the kind, because a memory that is a rule about its own kind is a rule
/// that has to be right twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FactKind {
    /// True of the world.
    World,
    /// True of what happened.
    Experience,
}

/// One extracted claim, with the exact text it came from.
///
/// The `quote` is what makes a fact arguable: it is the sentence as the
/// thread wrote it, in the thread's language, and every observation cites it
/// verbatim. Nothing here is translated, summarised or case-folded — only the
/// [matching key](crate::normalize_key) derived from the subject and predicate
/// is normalised, and that key is never stored instead of the text. The
/// `subject`, `predicate` and `object` are the claim as written; `scope` is
/// what a read is checked against.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// Assigned by [`MemoryStore::retain`](crate::MemoryStore::retain); empty
    /// on a fact that has not been retained yet.
    pub id: String,
    /// Retention order, assigned by `retain`. Consolidation breaks a tie
    /// between two facts said in the same second by this, not by id.
    pub(crate) seq: u64,
    pub kind: FactKind,
    pub scope: Scope,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// The exact source text, redacted on the way in.
    pub quote: String,
    /// A `thread:…` or `message:…` reference the quote can be checked against.
    pub source: String,
    /// When it was said, in unix seconds.
    pub at: i64,
    /// An optional embedding of the quote, for semantic recall.
    pub embedding: Option<Vec<f32>>,
}

impl Fact {
    /// A fact with no quote, no source and no embedding — the minimum an
    /// extractor has, which is also the minimum the model can do something
    /// with. Chain [`quote`](Self::quote), [`source`](Self::source) and
    /// [`embedding`](Self::embedding) to fill in the rest.
    #[must_use]
    pub fn new(
        kind: FactKind,
        scope: Scope,
        subject: &str,
        predicate: &str,
        object: &str,
        at: i64,
    ) -> Self {
        Self {
            id: String::new(),
            seq: 0,
            kind,
            scope,
            subject: subject.to_owned(),
            predicate: predicate.to_owned(),
            object: object.to_owned(),
            quote: String::new(),
            source: String::new(),
            at,
            embedding: None,
        }
    }

    /// The exact source text this fact was extracted from.
    #[must_use]
    pub fn quote(mut self, quote: &str) -> Self {
        self.quote = quote.to_owned();
        self
    }

    /// A `thread:…` or `message:…` reference the quote can be checked against.
    #[must_use]
    pub fn source(mut self, source: &str) -> Self {
        self.source = source.to_owned();
        self
    }

    /// An embedding of the quote. Recall only compares embeddings of equal
    /// length; one of a different size is ignored rather than truncated.
    #[must_use]
    pub fn embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// The text keyword recall scores: the quote, plus the subject and object
    /// around it, because a fact retrieved only by its predicate is a fact
    /// nobody can find.
    #[must_use]
    pub fn searchable(&self) -> String {
        format!(
            "{} {} {} {}",
            self.quote, self.subject, self.predicate, self.object
        )
    }
}
