//! The branded shell for the device grant's browser pages (issue #72).
//!
//! **Why this exists.** `cratefield-module-device-auth` (pinned `=0.3.0`,
//! `src/handlers.rs`) renders its five browser documents — the code form,
//! the unknown-code page, the approval page and the two decision pages — as
//! bare, unstyled HTML with no stylesheet and no seam to supply one. Those
//! are the pages a person lands on from `livingbrain login`, on the same
//! origin as the web app, and they looked like an error. The module is
//! shared harness code, so this venture does not fork it: [`fetch`] hands
//! every HTML answer on a device-auth route to [`brand`], which re-renders it
//! inside the web app's own chrome — `/assets/app.css`, the
//! `livingbrain.wiki` header, a centred card — with Living Brain's copy.
//!
//! **How.** The module's markup is fixed by the pin, so [`brand`] recognises
//! each page by its `<title>` and lifts out the handful of values it
//! carries (the form actions, the `user_code`, the client and its label,
//! the scopes). Every lifted value is un-escaped and escaped again on the
//! way out, so nothing reaches the new page that the module had not already
//! escaped. A page it does not recognise — a later module version, a new
//! page — is never dropped: its original content is placed in the same
//! shell unchanged, so the worst a markup change can do is lose the copy,
//! never the form.
//!
//! **What is kept.** The forms post to exactly the actions the module
//! wrote, with exactly its hidden `user_code`; the code form keeps its
//! `GET` and input name. The response keeps every header the harness
//! stamped (`no-store`, `X-Frame-Options`, `frame-ancestors 'none'`, and
//! the referrer policy [`crate::page_referrer_policy`] sets) and gains one
//! more CSP, [`PAGE_CSP`], which only tightens: a browser enforces every
//! policy it is handed.
//!
//! [`fetch`]: crate::fetch

/// The app's design-token sheet, served by Workers static assets from
/// `app/assets/` on this same origin, so `style-src 'self'` covers it and
/// its fonts (`font-src 'self'`).
pub(crate) const STYLESHEETS: [&str; 2] = ["/assets/app.css", "/assets/device.css"];

/// The web app's inline theme bootstrap, byte for byte as it stands in
/// `app/index.html`, `settings.html` and `wiki.html`.
///
/// **Why the copy.** The app is dark by default, follows the system until
/// the person chooses, and remembers the choice in `localStorage` under
/// `lb-theme`; the stylesheet alone can only do the first two. These pages
/// share the app's origin and so its storage, and running the same script
/// before first paint makes a person who chose light see light here too.
/// Byte-identical so its hash in [`PAGE_CSP`] is the hash `app/_headers`
/// already allows; a test holds both in step.
pub(crate) const THEME_SCRIPT: &str = "\n      (function () {\n        try {\n          var t = localStorage.getItem(\"lb-theme\");\n          if (t !== \"light\" && t !== \"dark\") {\n            t = window.matchMedia(\"(prefers-color-scheme: light)\").matches\n              ? \"light\"\n              : \"dark\";\n          }\n          document.documentElement.dataset.theme = t;\n        } catch (e) {\n          document.documentElement.dataset.theme = \"dark\";\n        }\n      })();\n    ";

/// The policy a branded page carries beside the harness's own
/// `frame-ancestors 'none'`.
///
/// The harness stamps only `frame-ancestors` on `/v1`; a page that now
/// loads a stylesheet and runs a script gets the same narrow policy the
/// static app has in `app/_headers`: everything `'self'`, the one inline
/// script by hash, nothing inline otherwise. `form-action 'self'` still
/// admits the approve and deny posts and the code form's `GET`, and the
/// `303` to `/index.html` an anonymous approver is sent on.
pub(crate) const PAGE_CSP: &str = "default-src 'none'; \
     script-src 'sha256-GsRRMiFErXfPA/V4np3z0CtFFGhxjhelBLl+TjY8sYA='; \
     style-src 'self'; font-src 'self'; img-src 'self' data:; \
     form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// The device grant's mount, as `cratefield-module-device-auth` 0.3.0
/// routes it.
const MOUNT: &str = "/v1/device-auth";

/// The one client this venture declares (`DEVICE_CLIENT` in `lib.rs`), by
/// the name a person knows it by.
const CLI_CLIENT: (&str, &str) = ("livingbrain-cli", "Living Brain CLI");

/// Whether this request can be answered with one of the module's HTML
/// documents: `GET` of the page (code form, unknown code, approval) and the
/// two decision `POST`s. `HEAD` is left alone — it has no body to brand —
/// and so are `/code` and `/token`, which answer JSON to a machine.
pub(crate) fn is_document(method: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix(MOUNT) else {
        return false;
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if method.eq_ignore_ascii_case("GET") {
        rest.is_empty()
    } else if method.eq_ignore_ascii_case("POST") {
        rest == "/approve" || rest == "/deny"
    } else {
        false
    }
}

/// Whether a `Content-Type` value names HTML. Together with
/// [`is_document`] it is the whole gate on [`brand`]; `fetch` reads neither
/// the body nor anything else of a response that fails it. Everything else —
/// the module's problem+json refusals, an OAuth error body — passes through.
pub(crate) fn is_html(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("text/html"))
    })
}

/// Re-renders one of the module's documents inside the app's shell. Total:
/// a page it does not recognise is wrapped as it stands.
pub(crate) fn brand(body: &str) -> String {
    match Page::parse(body) {
        Some(page) => page.render(),
        None => fallback(body),
    }
}

/// The five documents the pinned module renders, with the values each one
/// carries — every string here already un-escaped, so [`escape`] on the way
/// out is the only escaping that matters.
#[derive(Debug, PartialEq, Eq)]
enum Page {
    /// "Connect a device": the form a signed-in person with no code sees.
    CodeForm { action: String },
    /// "Unknown code": the code names nothing pending.
    UnknownCode { retry: String },
    /// "Approve a device": the question, and its two forms.
    Approval {
        client: String,
        name: Option<String>,
        scopes: Option<String>,
        code: String,
        approve: Form,
        deny: Form,
    },
    /// "Device approved".
    Approved,
    /// "Request denied".
    Denied,
}

/// One of the approval page's two `<form method="post">`s: where it posts
/// and the hidden `user_code` it carries.
#[derive(Debug, PartialEq, Eq)]
struct Form {
    action: String,
    user_code: String,
}

impl Page {
    /// Recognises the module's markup by its `<title>`. `None` for anything
    /// whose title or fields do not have the pinned shape.
    fn parse(body: &str) -> Option<Self> {
        let title = between(body, "<title>", "</title>")?;
        let content = &body[body.find("</title>")?..];
        match title {
            "Connect a device" => Some(Self::CodeForm {
                action: attribute(content, "action")?,
            }),
            "Unknown code" => Some(Self::UnknownCode {
                retry: attribute(content, "href")?,
            }),
            "Approve a device" => Self::parse_approval(content),
            "Device approved" => Some(Self::Approved),
            "Request denied" => Some(Self::Denied),
            _ => None,
        }
    }

    fn parse_approval(content: &str) -> Option<Self> {
        let client = unescape(between(content, "<p><strong>", "</strong>")?);
        let name = between(content, "</strong>", "</p>")?
            .strip_prefix(" &mdash; ")
            .map(unescape);
        // The module writes the literal "no scopes" for an empty request.
        let scopes = between(content, "<p>Asking for: ", "</p>")
            .filter(|scopes| *scopes != "no scopes")
            .map(unescape);
        let code = unescape(between(content, "<code>", "</code>")?);
        let forms = forms(content);
        let [first, second] = <[Form; 2]>::try_from(forms).ok()?;
        let (approve, deny) = (first.action.ends_with("/approve")
            && second.action.ends_with("/deny"))
        .then_some((first, second))?;
        Some(Self::Approval {
            client,
            name,
            scopes,
            code,
            approve,
            deny,
        })
    }

    fn render(&self) -> String {
        match self {
            Self::CodeForm { action } => shell(
                "Connect a device",
                &format!(
                    "<p class=\"eyebrow\">Device sign-in</p>\
                     <h1 id=\"device-title\">Connect a device</h1>\
                     <p class=\"lede\">Enter the code shown in your terminal.</p>\
                     <form class=\"device__form\" method=\"get\" action=\"{action}\">\
                     <label for=\"user-code\">Code</label>\
                     <input id=\"user-code\" class=\"device__input\" name=\"user_code\" \
                     autocomplete=\"off\" autocapitalize=\"characters\" spellcheck=\"false\" \
                     placeholder=\"XXXX-XXXX\" required>\
                     <button type=\"submit\" data-variant=\"primary\">Continue</button>\
                     </form>",
                    action = escape(action),
                ),
            ),
            Self::UnknownCode { retry } => shell(
                "Unknown code",
                &format!(
                    "<p class=\"eyebrow\">Device sign-in</p>\
                     <h1 id=\"device-title\">That code is not valid</h1>\
                     <p class=\"lede\">It may have been typed wrong, already answered, \
                     or expired.</p>\
                     <p class=\"muted\">Codes last a few minutes. Run \
                     <code>livingbrain login</code> again for a fresh one.</p>\
                     <div class=\"device__actions\">\
                     <a class=\"btn btn--primary\" href=\"{retry}\">Try again</a></div>",
                    retry = escape(retry),
                ),
            ),
            Self::Approval {
                client,
                name,
                scopes,
                code,
                approve,
                deny,
            } => shell(
                "Approve a device",
                &approval(
                    client,
                    name.as_deref(),
                    scopes.as_deref(),
                    code,
                    approve,
                    deny,
                ),
            ),
            Self::Approved => shell(
                "Device approved",
                "<p class=\"pill\" data-state=\"live\">Approved</p>\
                 <h1 id=\"device-title\">Device approved</h1>\
                 <p class=\"lede\">You can close this tab and return to your terminal.</p>\
                 <p class=\"muted\">The CLI finishes signing in on its own. You can \
                 revoke its token at any time in \
                 <a href=\"/settings.html#tokens\">Settings → API tokens</a>.</p>",
            ),
            Self::Denied => shell(
                "Request denied",
                "<p class=\"pill\" data-state=\"warn\">Denied</p>\
                 <h1 id=\"device-title\">Request denied</h1>\
                 <p class=\"lede\">The device was not signed in. You can close this tab.</p>\
                 <p class=\"muted\">If that was a mistake, run \
                 <code>livingbrain login</code> again for a fresh code.</p>",
            ),
        }
    }
}

/// The approval page's body. The scopes line the module prints raw
/// ("Asking for: no scopes") becomes what approving actually grants: the
/// tokens module's issuer mints a personal access token for the person who
/// presses Approve, in the workspace they are signed in to, and that token
/// carries the person's own access.
fn approval(
    client: &str,
    name: Option<&str>,
    scopes: Option<&str>,
    code: &str,
    approve: &Form,
    deny: &Form,
) -> String {
    let known = (client == CLI_CLIENT.0).then_some(CLI_CLIENT.1);
    let who = escape(known.unwrap_or(client));
    let device = name
        .map(|name| format!("<div><dt>Device</dt><dd>{}</dd></div>", escape(name.trim())))
        .unwrap_or_default();
    let client_row = if known.is_some() {
        String::new()
    } else {
        format!(
            "<div><dt>Client</dt><dd><code>{}</code></dd></div>",
            escape(client)
        )
    };
    let grant = match scopes {
        Some(scopes) => format!(
            "A personal access token for your Living Brain account, limited to \
             <code>{}</code>.",
            escape(scopes)
        ),
        None => "A personal access token with full access to your Living Brain \
                 account from the command line."
            .to_owned(),
    };
    format!(
        "<p class=\"eyebrow\">Device sign-in</p>\
         <h1 id=\"device-title\">Approve {who}?</h1>\
         <p class=\"lede\">Check that this code matches the one in your terminal.</p>\
         <p class=\"device__code\"><span class=\"sr\">Code: </span><code>{code}</code></p>\
         <dl class=\"facts device__facts\">{client_row}{device}\
         <div><dt>Grants</dt><dd>{grant}</dd></div></dl>\
         <p class=\"muted\">You can revoke it at any time in Settings → API tokens. \
         If you did not just run <code>livingbrain login</code>, deny this.</p>\
         <div class=\"device__actions\">\
         <form method=\"post\" action=\"{approve_action}\">\
         <input type=\"hidden\" name=\"user_code\" value=\"{approve_code}\">\
         <button type=\"submit\" data-variant=\"primary\">Approve</button></form>\
         <form method=\"post\" action=\"{deny_action}\">\
         <input type=\"hidden\" name=\"user_code\" value=\"{deny_code}\">\
         <button type=\"submit\">Deny</button></form></div>",
        code = escape(code),
        approve_action = escape(&approve.action),
        approve_code = escape(&approve.user_code),
        deny_action = escape(&deny.action),
        deny_code = escape(&deny.user_code),
    )
}

/// A page [`Page::parse`] does not know, in the same shell. Its content is
/// the module's own output after `<title>` — already escaped by the module
/// and carrying its forms intact — so it is placed as it stands.
fn fallback(body: &str) -> String {
    let title =
        between(body, "<title>", "</title>").map_or_else(|| "Device sign-in".to_owned(), unescape);
    let content = body
        .find("</title>")
        .map_or(body, |at| &body[at + "</title>".len()..]);
    shell(
        &title,
        &format!("<div class=\"device__raw\">{content}</div>"),
    )
}

/// The app's chrome around one card: the same head as `app/index.html`
/// (stylesheet, theme bootstrap, colour scheme), the same header with the
/// `livingbrain.wiki` mark, and the card centred in `main`. The title is
/// escaped here; `card` is markup its caller built from escaped values.
fn shell(title: &str, card: &str) -> String {
    let stylesheets: String = STYLESHEETS
        .iter()
        .map(|href| format!("<link rel=\"stylesheet\" href=\"{href}\">"))
        .collect();
    format!(
        "<!doctype html>\n<html lang=\"en\"><head>\
         <meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} — Living Brain</title>\
         <meta name=\"color-scheme\" content=\"dark light\">\
         <meta name=\"theme-color\" media=\"(prefers-color-scheme: dark)\" content=\"#070f11\">\
         <meta name=\"theme-color\" media=\"(prefers-color-scheme: light)\" content=\"#f2f8f8\">\
         <link rel=\"icon\" href=\"/assets/favicon.svg\" type=\"image/svg+xml\">\
         {stylesheets}<script>{THEME_SCRIPT}</script></head>\
         <body data-page=\"device\">\
         <header class=\"hd\"><div class=\"wrap hd__in\">\
         <a class=\"brand\" href=\"/index.html\">\
         <span class=\"mark\" aria-hidden=\"true\"><span></span></span>\
         <span class=\"brand__word\">livingbrain<span>.wiki</span></span></a>\
         </div></header>\
         <main id=\"main\" class=\"wrap device\">\
         <section class=\"card device__card\" aria-labelledby=\"device-title\">{card}</section>\
         </main></body></html>\n",
        title = escape(title),
    )
}

/// The text between the first `start` and the next `end` after it.
fn between<'a>(haystack: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = haystack.find(start)? + start.len();
    let len = haystack[from..].find(end)?;
    Some(&haystack[from..from + len])
}

/// The first double-quoted `name="…"` attribute value, un-escaped.
fn attribute(haystack: &str, name: &str) -> Option<String> {
    between(haystack, &format!(" {name}=\""), "\"").map(unescape)
}

/// Every `<form method="post" …>` in order, with its action and the hidden
/// `user_code` inside it. A form missing either is skipped, which makes the
/// approval page's two-form check fail and the page fall back whole.
fn forms(content: &str) -> Vec<Form> {
    content
        .split("<form ")
        .skip(1)
        .filter_map(|form| {
            let form = &form[..form.find("</form>")?];
            let head = &form[..form.find('>')?];
            if !head.contains("method=\"post\"") {
                return None;
            }
            Some(Form {
                action: attribute(head, "action")?,
                user_code: between(form, "name=\"user_code\" value=\"", "\"").map(unescape)?,
            })
        })
        .collect()
}

/// The five characters that can end a piece of text early in HTML — the
/// same set the module escapes.
pub(crate) fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The inverse of the module's escaping, and of nothing else: any other
/// entity stays as written and is escaped again (its `&` becoming `&amp;`),
/// which shows it as text rather than interpreting it. `&amp;` goes last so
/// `&amp;lt;` reads back as the text `&lt;`, not `<`.
fn unescape(escaped: &str) -> String {
    escaped
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The approval page exactly as `cratefield-module-device-auth` 0.3.0's
    /// `approval_page` writes it.
    fn crate_approval(name: &str, scopes: &str, code: &str) -> String {
        format!(
            "<!doctype html><meta charset=utf-8><title>Approve a device</title>\
             <h1>Approve this device?</h1>\
             <p><strong>livingbrain-cli</strong>{name}</p>\
             <p>Asking for: {scopes}</p>\
             <p>Code: <code>{code}</code></p>\
             <form method=\"post\" action=\"/v1/device-auth/approve\">\
             <input type=\"hidden\" name=\"user_code\" value=\"{code}\">\
             <button type=\"submit\">Approve</button></form>\
             <form method=\"post\" action=\"/v1/device-auth/deny\">\
             <input type=\"hidden\" name=\"user_code\" value=\"{code}\">\
             <button type=\"submit\">Deny</button></form>"
        )
    }

    #[test]
    fn the_approval_page_keeps_both_forms_and_their_hidden_code() {
        let page = brand(&crate_approval("", "no scopes", "ABCD-EFGH"));
        assert!(page.contains(
            "<form method=\"post\" action=\"/v1/device-auth/approve\">\
             <input type=\"hidden\" name=\"user_code\" value=\"ABCD-EFGH\">"
        ));
        assert!(page.contains(
            "<form method=\"post\" action=\"/v1/device-auth/deny\">\
             <input type=\"hidden\" name=\"user_code\" value=\"ABCD-EFGH\">"
        ));
        assert!(page.contains("<code>ABCD-EFGH</code>"), "the code on show");
        assert!(page.contains("Approve Living Brain CLI?"));
        assert!(page.contains("full access to your Living Brain account from the command line"));
        assert!(!page.contains("no scopes"), "the raw scopes line is gone");
        assert!(page.contains("<link rel=\"stylesheet\" href=\"/assets/app.css\">"));
        assert!(page.contains("class=\"brand__word\">livingbrain<span>.wiki</span>"));
        assert!(page.contains("data-variant=\"primary\">Approve</button>"));
    }

    #[test]
    fn a_device_label_and_scopes_are_shown_and_escaped_once() {
        // The module's own escaping of a hostile label and scope list.
        let page = brand(&crate_approval(
            " &mdash; &lt;img src=x onerror=alert(1)&gt; &amp; &#39;co&quot;",
            "read&lt;script&gt;",
            "ABCD-EFGH",
        ));
        assert!(!page.contains("<img"), "{page}");
        assert!(!page.contains("<script>read") && !page.contains("read<script>"));
        assert!(page.contains("&lt;img src=x onerror=alert(1)&gt; &amp; &#39;co&quot;"));
        assert!(page.contains("limited to <code>read&lt;script&gt;</code>."));
        // Escaped exactly once: no double-escaped entity leaks through.
        assert!(!page.contains("&amp;lt;") && !page.contains("&amp;amp;"));
    }

    #[test]
    fn a_hostile_code_and_action_cannot_break_out_of_their_attributes() {
        let body = crate_approval("", "no scopes", "A&quot;&gt;&lt;b&gt;").replace(
            "/v1/device-auth/approve",
            "/v1/device-auth/x&quot;&gt;/approve",
        );
        let page = brand(&body);
        assert!(!page.contains("<b>"), "{page}");
        assert!(page.contains("value=\"A&quot;&gt;&lt;b&gt;\""));
        assert!(page.contains("action=\"/v1/device-auth/x&quot;&gt;/approve\""));
    }

    #[test]
    fn the_code_form_keeps_its_get_action_and_input_name() {
        let page = brand(
            "<!doctype html><meta charset=utf-8><title>Connect a device</title>\
             <h1>Connect a device</h1><p>Enter the code shown on your device.</p>\
             <form method=\"get\" action=\"/v1/device-auth/\">\
             <input name=\"user_code\" autocomplete=\"off\" autocapitalize=\"characters\" \
             spellcheck=\"false\" placeholder=\"XXXX-XXXX\" required>\
             <button type=\"submit\">Continue</button></form>",
        );
        assert!(
            page.contains(
                "<form class=\"device__form\" method=\"get\" action=\"/v1/device-auth/\">"
            )
        );
        assert!(page.contains("name=\"user_code\""));
        assert!(page.contains(">Continue</button>"));
    }

    #[test]
    fn the_unknown_code_page_keeps_its_retry_link() {
        let page = brand(
            "<!doctype html><meta charset=utf-8><title>Unknown code</title>\
             <h1>That code is not valid</h1>\
             <p>It may have been typed wrong, already answered, or expired.</p>\
             <p><a href=\"/v1/device-auth\">Try again</a></p>",
        );
        assert!(page.contains("href=\"/v1/device-auth\">Try again</a>"));
        assert!(page.contains("That code is not valid"));
    }

    #[test]
    fn the_decision_pages_say_what_to_do_next() {
        let approved = brand(
            "<!doctype html><meta charset=utf-8><title>Device approved</title>\
             <h1>Device approved</h1><p>You can return to your terminal.</p>",
        );
        assert!(approved.contains("You can close this tab and return to your terminal."));
        let denied = brand(
            "<!doctype html><meta charset=utf-8><title>Request denied</title>\
             <h1>Request denied</h1><p>The device was not connected.</p>",
        );
        assert!(denied.contains("<h1 id=\"device-title\">Request denied</h1>"));
        assert!(denied.contains("You can close this tab."));
    }

    /// A page this venture does not know is wrapped as it stands — its
    /// forms survive — rather than dropped or mangled.
    #[test]
    fn an_unknown_page_is_wrapped_with_its_markup_intact() {
        let original = "<form method=\"post\" action=\"/v1/device-auth/new\">\
                        <input type=\"hidden\" name=\"x\" value=\"y\"></form>";
        let page = brand(&format!(
            "<!doctype html><meta charset=utf-8><title>Something &amp; new</title>{original}"
        ));
        assert!(page.contains(original));
        assert!(page.contains("<title>Something &amp; new — Living Brain</title>"));
        assert!(page.contains("/assets/app.css"));
        // An approval page whose forms are not the expected pair falls
        // back too, keeping them rather than re-rendering a guess.
        let odd = crate_approval("", "no scopes", "ABCD-EFGH").replace("/deny", "/other");
        let page = brand(&odd);
        assert!(page.contains("action=\"/v1/device-auth/other\""));
        assert!(page.contains("class=\"device__raw\""));
    }

    /// `fetch`'s gate, as it applies it: the route first, then the type.
    fn branded(method: &str, path: &str, content_type: Option<&str>, body: &str) -> Option<String> {
        (is_document(method, path) && is_html(content_type)).then(|| brand(body))
    }

    #[test]
    fn only_device_documents_that_answer_html_are_branded() {
        let html = Some("text/html; charset=utf-8");
        for (method, path) in [
            ("GET", "/v1/device-auth"),
            ("GET", "/v1/device-auth/"),
            ("POST", "/v1/device-auth/approve"),
            ("POST", "/v1/device-auth/deny"),
        ] {
            assert!(branded(method, path, html, "").is_some(), "{method} {path}");
        }
        for (method, path) in [
            ("HEAD", "/v1/device-auth"),
            ("POST", "/v1/device-auth"),
            ("POST", "/v1/device-auth/code"),
            ("POST", "/v1/device-auth/token"),
            ("GET", "/v1/device-auth/approve"),
            ("GET", "/v1/device-authx"),
            ("GET", "/v1/workspaces/email/verify"),
            ("GET", "/index.html"),
        ] {
            assert!(branded(method, path, html, "").is_none(), "{method} {path}");
        }
        for content_type in [
            None,
            Some("application/problem+json"),
            Some("application/json"),
            Some("text/htmlx"),
        ] {
            assert!(
                branded("POST", "/v1/device-auth/approve", content_type, "{}").is_none(),
                "{content_type:?}"
            );
        }
        assert!(is_html(Some("TEXT/HTML")));
    }

    /// The policy's script hash is the hash of [`THEME_SCRIPT`], and that
    /// script is the app's own, byte for byte, with the hash `_headers`
    /// allows — so editing either side without the other fails here, not in
    /// a browser that silently refuses the script.
    #[test]
    fn the_theme_script_and_its_hash_match_the_app() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        let hash = base64::engine::general_purpose::STANDARD
            .encode(Sha256::digest(THEME_SCRIPT.as_bytes()));
        let source = format!("'sha256-{hash}'");
        assert!(PAGE_CSP.contains(&source), "{source}");
        let index = include_str!("../../../app/index.html");
        assert!(index.contains(&format!("<script>{THEME_SCRIPT}</script>")));
        assert!(include_str!("../../../app/_headers").contains(&source));
        for href in STYLESHEETS {
            let file = href.trim_start_matches('/');
            assert!(
                std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../app"))
                    .join(file)
                    .is_file(),
                "{href} is not a file under app/"
            );
        }
        assert!(PAGE_CSP.contains("frame-ancestors 'none'"));
        assert!(!PAGE_CSP.contains("unsafe"));
    }
}
