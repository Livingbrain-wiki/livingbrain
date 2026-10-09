//! The problems this surface answers with, and the mapping from a
//! [`PageError`] to the one a caller should see.
//!
//! The definitions follow `livingbrain-models/src/handlers.rs`: one local
//! `ProblemDef` per failure shape, slug prefixed with the surface's name.
//! Store internals never reach a body — a caller can act on a conflict, a
//! missing page or a refused frontmatter, and can do nothing with a key
//! version or a SQL message, so everything else is [`Problem::internal`].

use cratefield_core::axum::http::StatusCode;
use cratefield_core::{Problem, ProblemDef};
use livingbrain_pages::PageError;

/// A missing or rejected credential — one definition for both, so the body
/// says the same thing either way and a stranger learns nothing about
/// whether a token exists. The detail tells the caller what to do about it.
pub(crate) const UNAUTHORIZED: ProblemDef = ProblemDef {
    slug: "api/unauthorized",
    status: StatusCode::UNAUTHORIZED,
    title: "Bearer token required",
    description: "This endpoint answers only with a valid bearer token, and every \
                  read and write is scoped to the user it speaks for. Sign in with \
                  `livingbrain login` and send `Authorization: Bearer <token>`.",
};

/// A slug no readable page carries. Absent and owned-by-someone-else are
/// the same answer, the MCP server's rule: saying which would be an answer
/// about somebody else's page.
pub(crate) const PAGE_NOT_FOUND: ProblemDef = ProblemDef {
    slug: "api/page-not-found",
    status: StatusCode::NOT_FOUND,
    title: "No such page",
    description: "No page the caller can read carries that slug. It may not exist, \
                  or it may belong to somebody else — this surface does not say which.",
};

/// The page moved under the caller: a stale `base_version`, an edit with no
/// base at all, or a human edit waiting for reconciliation. Always the same
/// advice — reload, then re-apply — which is what the web app tells its
/// editor on a 409.
pub(crate) const PAGE_CONFLICT: ProblemDef = ProblemDef {
    slug: "api/page-conflict",
    status: StatusCode::CONFLICT,
    title: "The page moved",
    description: "Nothing was written: the page changed since the caller last read it. \
                  Reload the page to get the current version, then re-apply the edit.",
};

/// A write the store refused over the page's own rules: frontmatter that
/// does not fit the entity type, a link to something that cannot be a page,
/// a type that would have to change. The detail carries the store's reason.
pub(crate) const PAGE_REFUSED: ProblemDef = ProblemDef {
    slug: "api/page-refused",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "The page was refused",
    description: "The page store refused this write; the `detail` carries the reason. \
                  Nothing was stored.",
};

/// Content redaction could not handle. Refused *before* storing, so nothing
/// about it — not even the redacted remainder — was ever written.
pub(crate) const REDACTION_REFUSED: ProblemDef = ProblemDef {
    slug: "api/redaction-refused",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Content refused before storing",
    description: "Nothing was written: the content held something redaction refused \
                  to handle, and it was refused before anything was stored.",
};

/// An export format other than the one this surface speaks.
pub(crate) const EXPORT_FORMAT: ProblemDef = ProblemDef {
    slug: "api/unknown-export-format",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "Unknown export format",
    description: "The export speaks one format: `obsidian`. Name it with \
                  `?format=obsidian`, or send no `format` at all.",
};

/// The problem for content redaction refused, in one place so every write
/// path says the same thing.
pub(crate) fn redaction_refused() -> Problem {
    Problem::new(&REDACTION_REFUSED)
}

/// The problem for a slug outside the slug rule on the way in (a `PUT`): a
/// refused body rather than a missing page.
pub(crate) fn slug_refused() -> Problem {
    Problem::new(&PAGE_REFUSED).with_detail(
        "that slug is outside the slug rule: lowercase ascii letters, \
                      digits and `-`, 1..=128 characters.",
    )
}

/// The problem for a slug outside the slug rule on the way out (a `GET`):
/// from where the caller sits it is just a page that is not there.
pub(crate) fn slug_not_found() -> Problem {
    Problem::new(&PAGE_NOT_FOUND)
}

/// Maps a read failure to the problem the caller sees.
pub(crate) fn page_read_error(error: PageError) -> Problem {
    page_error(error, slug_not_found())
}

/// Maps a write failure to the problem the caller sees.
pub(crate) fn page_write_error(error: PageError) -> Problem {
    page_error(error, slug_refused())
}

/// The shared mapping. Read and write disagree about exactly one case — an
/// invalid slug is a missing page on a read and a refused body on a write —
/// so the caller hands that one in.
fn page_error(error: PageError, invalid_slug: Problem) -> Problem {
    match error {
        PageError::InvalidSlug(_) => invalid_slug,
        PageError::Shredded(_) => Problem::new(&PAGE_NOT_FOUND)
            .with_detail("that memory has been deleted; nothing is there to read."),
        PageError::Conflict { base, head } => Problem::new(&PAGE_CONFLICT).with_detail(format!(
            "nothing was written: the page moved to version {head} since you read it \
             (you wrote against {base}). Reload the page to get the current version, \
             then re-apply your edit."
        )),
        PageError::HumanEditPending { version } => {
            Problem::new(&PAGE_CONFLICT).with_detail(format!(
                "nothing was written: that page has a human edit at version {version} \
                 waiting. Reload the page, reconcile, and try again."
            ))
        }
        PageError::MissingBaseVersion => Problem::new(&PAGE_CONFLICT).with_detail(
            "nothing was written: editing a page that exists requires the \
             base_version you read it at. Reload the page to get the current version.",
        ),
        PageError::NewPageWithBase { base } => Problem::new(&PAGE_REFUSED).with_detail(format!(
            "a new page cannot carry a base_version (got {base}); leave it null to \
             create one"
        )),
        PageError::Frontmatter(message) => {
            Problem::new(&PAGE_REFUSED).with_detail(format!("the page was not stored: {message}"))
        }
        PageError::InvalidLink(target) => Problem::new(&PAGE_REFUSED).with_detail(format!(
            "the page was not stored: [[{target}]] is not a slug, so it cannot be a \
             link target"
        )),
        PageError::EntityTypeChanged { from, to } => {
            Problem::new(&PAGE_REFUSED).with_detail(format!(
                "the page was not stored: a page's entity type cannot change \
                 ({} -> {})",
                from.as_str(),
                to.as_str()
            ))
        }
        PageError::TypeMismatch { declared, found } => {
            Problem::new(&PAGE_REFUSED).with_detail(format!(
                "the page was not stored: frontmatter type `{}` does not match the \
                 declared `{}`",
                found.as_str(),
                declared.as_str()
            ))
        }
        // Key versions, corruption, the custodian, the blob store and the
        // database are the operator's problems, not the caller's: the 500
        // says nothing about which it was.
        _ => Problem::internal(),
    }
}
