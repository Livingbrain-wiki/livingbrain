//! The sanctioned bridge from a module's own blob view onto the `pages` key
//! space.
//!
//! **Why per request, and never at compose time:** workerd ties an
//! env-derived I/O object to the request context that created it. A blob
//! handle built once when the venture composed its modules — the
//! `Wiki { blob, … }` this crate used to carry — is a handle from *some*
//! earlier request, and reusing it on a later request fails with
//! `Cannot perform I/O on behalf of a different request context`. The
//! runtime re-derives every binding per request (`ports()` inside
//! `serve`), so the pages key space must be reached through a handle that
//! is derived the same way — this module's own `ctx.ports.blob` — and
//! never through one parked in a constructor.
//!
//! **Where the adapter sits:** the harness scopes a module's `Blob` port to
//! the module's own name (`ScopedBlob` over the raw store, keyed by
//! `module.name()`), and that wrapper's prefix is not something a module
//! can reach past. So the composition plants the rebind *under* it — over
//! the raw per-request store, at the port layer. [`PagesRebind`] is that
//! planting for the whole bundle:
//!
//! ```text
//! ports.blob = Some(Arc::new(PagesRebind::new(raw_per_request_store)));
//! // the harness then hands the notes module: ScopedBlob(PagesRebind(raw), "notes")
//! ```
//!
//! — which is [`RebindBlob`] under each module's scope, one per name this
//! crate mounts.
//!
//! A write the page store makes (`{scope}/{slug}/{version}-….md`) crosses
//! the module's scope first (`notes/{scope}/…`), is rewritten here to the
//! owner's key space (`pages/{scope}/…`), and lands on the same physical
//! objects the pages module writes — through a handle derived this request.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cratefield_core::{Blob, BlobError, BlobObject, PresignedPut, check_blob_size};

/// A blob view that forwards to another module's key space: keys arriving
/// already scoped to THIS module (`<module>/<key>`) are rewritten to
/// `<owner>/<key>` before hitting `inner`. This is the sanctioned bridge for
/// a module that reads page bodies owned by the `pages` module — the same
/// physical objects, through the module's own per-request view, so no I/O
/// handle is ever cached across requests (see the [module
/// docs](self#why-per-request-and-never-at-compose-time) for the workerd
/// lifetime that forbids that).
///
/// The keys arriving here are **already prefixed with `from`** — they come
/// through that module's `ScopedBlob`, which is why the adapter sits between
/// the raw store and the scope, not between the scope and the caller. A key
/// without the prefix is [`BlobError::BadKey`]: this view speaks only for
/// its owner's key space, and a bare key has no meaning here.
pub struct RebindBlob {
    /// The store the rebound keys hit: the **raw** per-request blob the
    /// composition resolved for this request, not a scoped view of it.
    inner: Arc<dyn Blob>,
    /// The caller's module name — the prefix incoming keys carry.
    from: &'static str,
    /// The owner of the key space the operation lands in (`pages`).
    to: &'static str,
}

impl RebindBlob {
    /// Rebinds the `from` key space onto `to`: `"{from}/{key}"` arrives,
    /// `"{to}/{key}"` reaches `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn Blob>, from: &'static str, to: &'static str) -> Self {
        Self { inner, from, to }
    }

    /// Maps an incoming (already module-scoped) key onto the owner's key
    /// space, refusing one that is not the caller's to touch.
    fn rebind(&self, key: &str) -> Result<String, BlobError> {
        let scoped = format!("{}/", self.from);
        match key.strip_prefix(scoped.as_str()) {
            Some(rest) => Ok(format!("{}/{}", self.to, rest)),
            None => Err(BlobError::BadKey(format!(
                "key `{key}` is not scoped to `{}`; a module reaches the `{}` key space only \
                 through its own scoped view, rebound by the composition",
                self.from, self.to,
            ))),
        }
    }
}

#[async_trait]
impl Blob for RebindBlob {
    async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), BlobError> {
        check_blob_size(bytes)?;
        self.inner
            .put(&self.rebind(key)?, bytes, content_type)
            .await
    }

    async fn get(&self, key: &str) -> Result<Option<BlobObject>, BlobError> {
        self.inner.get(&self.rebind(key)?).await
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        self.inner.delete(&self.rebind(key)?).await
    }

    async fn signed_url(&self, key: &str, ttl: Duration) -> Result<String, BlobError> {
        self.inner.signed_url(&self.rebind(key)?, ttl).await
    }

    async fn signed_put_url(
        &self,
        key: &str,
        content_type: &str,
        content_length: Option<u64>,
        ttl: Duration,
    ) -> Result<PresignedPut, BlobError> {
        let key = self.rebind(key)?;
        self.inner
            .signed_put_url(&key, content_type, content_length, ttl)
            .await
    }
}

/// The whole port layer, rooted on the pages key space: every key one of
/// this crate's four modules writes arrives carrying that module's own scope
/// (`notes/…`, `search/…`, `ask/…`, `export/…` — the harness's [`ScopedBlob`
/// keyed it) and is rewritten to `pages/…`; every other key passes through
/// untouched, so the `pages` module's own `pages/…` keys land exactly as
/// they always did.
///
/// This is the composition's one port-layer planting, in the shape the
/// [crate docs](crate) describe: the runtime resolves a bundle per request,
/// the composition swaps its raw store for this wrapper,
///
/// ```text
/// ports.blob = Some(Arc::new(PagesRebind::new(raw_per_request_store)));
/// ```
///
/// and [`cratefield_core::Ports::view_for`] then hands each module
/// `ScopedBlob(PagesRebind(raw), module)` — [`RebindBlob`] under the scope,
/// per module, derived this request. Keys outside the four prefixes (a
/// future module that declares `Port::Blob`, or a scope whose first segment
/// merely starts alike) reach `inner` unchanged: the wrapper speaks only
/// for the four names this crate mounts.
pub struct PagesRebind {
    /// The store every non-sibling key hits unchanged — the raw per-request
    /// store the runtime resolved, and the `pages` module's own path.
    inner: Arc<dyn Blob>,
    /// The per-module rebinds, one per name this crate mounts.
    rebinds: [(&'static str, RebindBlob); 4],
}

impl PagesRebind {
    /// Roots the four sibling modules' scopes on the pages key space, over
    /// the raw per-request store.
    #[must_use]
    pub fn new(inner: Arc<dyn Blob>) -> Self {
        Self {
            rebinds: [
                (
                    "notes",
                    RebindBlob::new(Arc::clone(&inner), "notes", "pages"),
                ),
                (
                    "search",
                    RebindBlob::new(Arc::clone(&inner), "search", "pages"),
                ),
                ("ask", RebindBlob::new(Arc::clone(&inner), "ask", "pages")),
                (
                    "export",
                    RebindBlob::new(Arc::clone(&inner), "export", "pages"),
                ),
            ],
            inner,
        }
    }

    /// The rebind the first key segment names, if it is one of this crate's
    /// modules. Matching the whole segment (`notes/…`, never a prefix of a
    /// longer name) is what keeps `notesother/…` out of the notes key space.
    fn sibling(&self, key: &str) -> Option<&RebindBlob> {
        let (module, _) = key.split_once('/')?;
        self.rebinds
            .iter()
            .find(|(name, _)| *name == module)
            .map(|(_, rebind)| rebind)
    }
}

#[async_trait]
impl Blob for PagesRebind {
    async fn put(&self, key: &str, bytes: &[u8], content_type: &str) -> Result<(), BlobError> {
        check_blob_size(bytes)?;
        match self.sibling(key) {
            Some(rebind) => rebind.put(key, bytes, content_type).await,
            None => self.inner.put(key, bytes, content_type).await,
        }
    }

    async fn get(&self, key: &str) -> Result<Option<BlobObject>, BlobError> {
        match self.sibling(key) {
            Some(rebind) => rebind.get(key).await,
            None => self.inner.get(key).await,
        }
    }

    async fn delete(&self, key: &str) -> Result<(), BlobError> {
        match self.sibling(key) {
            Some(rebind) => rebind.delete(key).await,
            None => self.inner.delete(key).await,
        }
    }

    async fn signed_url(&self, key: &str, ttl: Duration) -> Result<String, BlobError> {
        match self.sibling(key) {
            Some(rebind) => rebind.signed_url(key, ttl).await,
            None => self.inner.signed_url(key, ttl).await,
        }
    }

    async fn signed_put_url(
        &self,
        key: &str,
        content_type: &str,
        content_length: Option<u64>,
        ttl: Duration,
    ) -> Result<PresignedPut, BlobError> {
        match self.sibling(key) {
            Some(rebind) => {
                rebind
                    .signed_put_url(key, content_type, content_length, ttl)
                    .await
            }
            None => {
                self.inner
                    .signed_put_url(key, content_type, content_length, ttl)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PagesRebind, RebindBlob};
    use cratefield_core::{Blob, BlobError, PresignedPut, ScopedBlob};
    use cratefield_testing::MemoryBlob;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    /// A store that records the keys its presign methods were reached with,
    /// so a test can assert the *rebound* key arrived — and that a refused
    /// one never did. `signed_url` answers a URL that names the key, the way
    /// [`MemoryBlob`](cratefield_testing::MemoryBlob) names it, so the
    /// round-trip assertions read the same on both.
    #[derive(Default)]
    struct RecordingBlob {
        presigned: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Blob for RecordingBlob {
        async fn put(&self, _key: &str, _bytes: &[u8], _ct: &str) -> Result<(), BlobError> {
            Ok(())
        }
        async fn get(&self, _key: &str) -> Result<Option<cratefield_core::BlobObject>, BlobError> {
            Ok(None)
        }
        async fn delete(&self, _key: &str) -> Result<(), BlobError> {
            Ok(())
        }
        async fn signed_url(&self, key: &str, _ttl: Duration) -> Result<String, BlobError> {
            self.presigned.lock().unwrap().push(key.to_owned());
            Ok(format!("mem://{key}"))
        }
        async fn signed_put_url(
            &self,
            key: &str,
            content_type: &str,
            _len: Option<u64>,
            _ttl: Duration,
        ) -> Result<PresignedPut, BlobError> {
            self.presigned.lock().unwrap().push(key.to_owned());
            Ok(PresignedPut {
                url: format!("mem://{key}"),
                method: "PUT",
                headers: vec![("content-type".to_owned(), content_type.to_owned())],
            })
        }
    }

    #[pollster::test]
    async fn a_rebound_write_lands_on_the_owner_key_space_and_not_the_callers() {
        let memory = Arc::new(MemoryBlob::new());
        let rebind = RebindBlob::new(memory.clone(), "notes", "pages");

        // The key arrives already scoped to the caller: it crossed the
        // module's own ScopedBlob on its way here.
        rebind
            .put(
                "notes/w:WS/u:a/note/0000000001-x.md",
                b"body",
                "text/markdown",
            )
            .await
            .expect("a rebound write");

        // The physical object is the pages module's — and only there.
        let stored = memory
            .get("pages/w:WS/u:a/note/0000000001-x.md")
            .await
            .expect("a read");
        assert!(stored.is_some(), "the body landed under pages/");
        assert_eq!(stored.expect("the body").bytes, b"body");
        assert!(
            memory
                .get("notes/w:WS/u:a/note/0000000001-x.md")
                .await
                .expect("a read")
                .is_none(),
            "nothing landed under the caller's own prefix"
        );
    }

    #[pollster::test]
    async fn a_rebound_read_opens_what_the_owner_key_space_holds() {
        let memory = Arc::new(MemoryBlob::new());
        memory
            .put(
                "pages/w:WS/u:a/note/0000000001-x.md",
                b"body",
                "text/markdown",
            )
            .await
            .expect("a seed write the pages module could have made");
        let rebind = RebindBlob::new(memory, "notes", "pages");

        let read = rebind
            .get("notes/w:WS/u:a/note/0000000001-x.md")
            .await
            .expect("a rebound read")
            .expect("the page body the pages module wrote");
        assert_eq!(read.bytes, b"body");
    }

    #[pollster::test]
    async fn a_key_outside_the_callers_scope_is_refused_before_the_store() {
        let memory = Arc::new(MemoryBlob::new());
        let rebind = RebindBlob::new(memory.clone(), "notes", "pages");

        for bad in [
            "w:WS/u:a/note.md",            // bare: never went through the scope
            "search/w:WS/u:a/note.md",     // another module's scope
            "notesother/w:WS/u:a/note.md", // a prefix that merely starts alike
        ] {
            let error = rebind.get(bad).await.expect_err("refused");
            assert!(matches!(error, BlobError::BadKey(_)), "`{bad}`: {error}");
        }
        rebind
            .put("w:WS/u:a/note.md", b"body", "text/markdown")
            .await
            .expect_err("a bare put is refused too");
        assert!(
            memory.is_empty(),
            "the store never saw a key the adapter refused"
        );
    }

    #[pollster::test]
    async fn a_delete_lands_on_the_rebound_key() {
        let memory = Arc::new(MemoryBlob::new());
        memory
            .put("pages/w:WS/u:a/note.md", b"body", "text/markdown")
            .await
            .expect("a seed write");
        let rebind = RebindBlob::new(memory.clone(), "notes", "pages");

        rebind
            .delete("notes/w:WS/u:a/note.md")
            .await
            .expect("a rebound delete");

        assert!(
            memory
                .get("pages/w:WS/u:a/note.md")
                .await
                .expect("a read")
                .is_none(),
            "the owner key space lost the object"
        );
    }

    #[pollster::test]
    async fn a_signed_url_names_the_rebound_key() {
        let recorder = Arc::new(RecordingBlob::default());
        let rebind = RebindBlob::new(recorder.clone(), "export", "pages");
        let ttl = Duration::from_secs(60);

        let url = rebind
            .signed_url("export/w:WS/u:a/page.md", ttl)
            .await
            .expect("a signed url");
        assert_eq!(url, "mem://pages/w:WS/u:a/page.md", "the owner's key");

        let put = rebind
            .signed_put_url("export/w:WS/u:a/page.md", "text/markdown", None, ttl)
            .await
            .expect("a signed upload");
        assert_eq!(put.url, "mem://pages/w:WS/u:a/page.md", "the owner's key");

        assert_eq!(
            *recorder.presigned.lock().unwrap(),
            vec![
                "pages/w:WS/u:a/page.md".to_owned(),
                "pages/w:WS/u:a/page.md".to_owned(),
            ],
            "the store was reached with rebound keys only"
        );
    }

    #[pollster::test]
    async fn a_refused_presign_never_reaches_the_store() {
        let recorder = Arc::new(RecordingBlob::default());
        let rebind = RebindBlob::new(recorder, "notes", "pages");

        let error = rebind
            .signed_url("pages/w:WS/u:a/page.md", Duration::from_secs(60))
            .await
            .expect_err("a key carrying the owner's prefix is still not this module's");
        assert!(matches!(error, BlobError::BadKey(_)), "{error}");
    }

    /// The chain the venture plants: the harness's scope over the bundle
    /// wrapper. Every sibling module's write lands on the pages key space,
    /// through its own scope.
    #[pollster::test]
    async fn a_planted_bundle_roots_every_sibling_scope_on_pages() {
        let memory = Arc::new(MemoryBlob::new());
        let planted = Arc::new(PagesRebind::new(memory.clone()));

        for module in ["notes", "search", "ask", "export"] {
            let scoped = ScopedBlob::new(Arc::clone(&planted) as Arc<dyn Blob>, module);
            // The key as the page store writes it: relative, because the
            // scope has not crossed it yet.
            let key = format!("w:WS/u:a/{module}/0000000001-x.md");
            scoped
                .put(&key, b"body", "text/markdown")
                .await
                .expect("a write through the planted view");
            let physical = format!("pages/w:WS/u:a/{module}/0000000001-x.md");
            assert!(
                memory.get(&physical).await.expect("a read").is_some(),
                "{module}'s write landed at {physical}"
            );
            let own = format!("{module}/w:WS/u:a/{module}/0000000001-x.md");
            assert!(
                memory.get(&own).await.expect("a read").is_none(),
                "nothing landed under {module}/"
            );
        }
    }

    /// The pages module's own view crosses the same wrapper untouched: its
    /// scope already names the owner key space, and a passthrough keeps it
    /// that way.
    #[pollster::test]
    async fn the_pages_scope_passes_through_the_planted_bundle() {
        let memory = Arc::new(MemoryBlob::new());
        let planted: Arc<dyn Blob> = Arc::new(PagesRebind::new(memory.clone()));
        let scoped = ScopedBlob::new(planted, "pages");

        scoped
            .put("w:WS/u:a/page/0000000001-x.md", b"body", "text/markdown")
            .await
            .expect("a write through the pages view");

        let physical = "pages/w:WS/u:a/page/0000000001-x.md";
        assert!(
            memory.get(physical).await.expect("a read").is_some(),
            "the pages module's write landed at {physical}"
        );
    }

    /// A module the wrapper does not speak for reaches the raw store
    /// unchanged — and a name that merely starts alike is not a sibling.
    #[pollster::test]
    async fn a_stranger_scope_and_a_lookalike_pass_through_unchanged() {
        let memory = Arc::new(MemoryBlob::new());
        let planted: Arc<dyn Blob> = Arc::new(PagesRebind::new(memory.clone()));

        for key in ["notesother/w:WS/u:a/x.md", "models/w:WS/u:a/x.md"] {
            planted
                .put(key, b"body", "text/markdown")
                .await
                .expect("a passthrough write");
            assert!(
                memory.get(key).await.expect("a read").is_some(),
                "{key} landed unchanged"
            );
        }
    }
}
