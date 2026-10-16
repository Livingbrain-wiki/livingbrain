//! The pure parts of the connector sign-in (issue #72): PKCE, the redirect
//! URI rule, query encoding, and the consent page's markup. Nothing here
//! touches storage, so each rule is tested on its own.

use cratefield_core::axum::http::{HeaderValue, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// The one challenge method this server accepts (RFC 7636 §4.2). `plain` is
/// refused: a plain challenge is the verifier itself, so it binds nothing.
pub(crate) const S256: &str = "S256";

/// RFC 7636 §4.1: a verifier is 43 to 128 characters of the unreserved set.
pub(crate) fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// A challenge is the same shape as a verifier: 43 characters of base64url
/// (the S256 digest is 32 bytes, which is 43 unpadded characters).
pub(crate) fn valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// `BASE64URL(SHA256(verifier))`, the S256 transform of RFC 7636 §4.2.
pub(crate) fn s256(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// Whether `verifier` is the one `challenge` was made from. The comparison is
/// constant-time over the challenge's length.
pub(crate) fn verifier_matches(verifier: &str, challenge: &str) -> bool {
    valid_verifier(verifier)
        && cratefield_core::constant_time_eq(s256(verifier).as_bytes(), challenge.as_bytes())
}

/// Unpadded URL-safe base64 (RFC 4648 §5), written out so the module carries
/// no encoder dependency for one function.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[(n >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(n >> 12) as usize & 63]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[(n >> 6) as usize & 63]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[n as usize & 63]));
        }
    }
    out
}

/// A redirect URI the client may return to: absolute, without a fragment,
/// and either `https`, or `http` to this machine (`localhost`, `127.0.0.1`,
/// `[::1]`) for a client running locally. Anything else — a plain `http` URL
/// on another host, a `javascript:` or custom scheme, a fragment — is refused
/// at registration, so a code can only ever be sent to an address the client
/// registered as its own.
pub(crate) fn valid_redirect_uri(uri: &str) -> bool {
    if uri.len() > 512
        || uri.contains('#')
        || uri.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().map(|h| format!("[{h}]"))
    } else {
        authority.split(':').next().map(str::to_owned)
    }
    .unwrap_or_default();
    match scheme {
        "https" => !host.is_empty(),
        "http" => matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"),
        _ => false,
    }
}

/// The origin of a redirect URI, for the consent page's `form-action`
/// allowance: `scheme://authority`.
pub(crate) fn origin_of_uri(uri: &str) -> Option<String> {
    let (scheme, rest) = uri.split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    Some(format!("{scheme}://{authority}"))
}

/// RFC 3986 percent-encoding of one query value: unreserved bytes pass,
/// everything else is `%XX`.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `uri` with `params` appended to its query, `?` or `&` as the URI already
/// requires. The result is what an authorization response redirects to.
pub(crate) fn with_params(uri: &str, params: &[(&str, &str)]) -> String {
    let separator = if uri.contains('?') { '&' } else { '?' };
    let query: Vec<String> = params
        .iter()
        .map(|(name, value)| format!("{name}={}", encode(value)))
        .collect();
    format!("{uri}{separator}{}", query.join("&"))
}

/// HTML escaping for the five characters that matter in text and in quoted
/// attribute values.
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

/// One choice in the workspace picker.
pub(crate) struct Choice<'a> {
    pub(crate) workspace_id: &'a str,
    pub(crate) name: &'a str,
}

/// The consent page's inputs, all of them already trusted: the client name is
/// the registered one, the redirect origin is validated, and `request` is the
/// sealed consent token the form posts back.
pub(crate) struct Consent<'a> {
    pub(crate) client_name: &'a str,
    pub(crate) redirect_origin: &'a str,
    pub(crate) request: &'a str,
    pub(crate) choices: &'a [Choice<'a>],
}

/// The consent page's Content-Security-Policy. Stylesheets come from this
/// origin, nothing runs as script, the form posts back to this origin, and
/// the browser may follow the form's redirect to the client's own redirect
/// origin (Chrome applies `form-action` to redirect targets too).
pub(crate) fn consent_csp(redirect_origin: &str) -> String {
    format!(
        "default-src 'none'; style-src 'self'; font-src 'self'; img-src 'self' data:; \
         form-action 'self' {redirect_origin}; base-uri 'none'; frame-ancestors 'none'"
    )
}

/// The consent page. Plain English, in the device page's own vocabulary, so
/// it reads as the same product as the CLI approval it sits beside.
pub(crate) fn consent_page(consent: &Consent<'_>) -> String {
    let client = escape(consent.client_name);
    let origin = escape(consent.redirect_origin);
    let request = escape(consent.request);
    let picker = if consent.choices.len() == 1 {
        let only = &consent.choices[0];
        format!(
            "<dt>Workspace</dt><dd>{name}<input type=\"hidden\" name=\"workspace_id\" \
             value=\"{id}\"></dd>",
            name = escape(only.name),
            id = escape(only.workspace_id),
        )
    } else {
        let options: String = consent
            .choices
            .iter()
            .map(|choice| {
                format!(
                    "<option value=\"{id}\">{name}</option>",
                    id = escape(choice.workspace_id),
                    name = escape(choice.name),
                )
            })
            .collect();
        format!(
            "<dt><label for=\"oauth-workspace\">Workspace</label></dt>\
             <dd><select id=\"oauth-workspace\" name=\"workspace_id\">{options}</select></dd>"
        )
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"strict-origin-when-cross-origin\">\n\
         <title>Connect {client} to Living Brain</title>\n\
         <link rel=\"stylesheet\" href=\"/assets/app.css\">\n\
         <link rel=\"stylesheet\" href=\"/assets/device.css\">\n</head>\n\
         <body>\n<main class=\"device\">\n\
         <p class=\"eyebrow\">Connect an app</p>\n\
         <h1 id=\"oauth-title\">Let {client} use your Living Brain?</h1>\n\
         <p class=\"lede\">{client} wants to read and add notes to your Living Brain.</p>\n\
         <dl class=\"facts device__facts\">\
         <div>{picker}</div>\
         <div><dt>Returns to</dt><dd><code>{origin}</code></dd></div>\
         <div><dt>Grants</dt><dd>Access to the pages you can read in this workspace, and \
         the ability to add notes.</dd></div></dl>\n\
         <p class=\"muted\">You can revoke this connection at any time in Settings → API \
         tokens. If you did not start this from {client}, deny it.</p>\n\
         <div class=\"device__actions\">\n\
         <form method=\"post\" action=\"/v1/oauth/authorize\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <button type=\"submit\" name=\"decision\" value=\"approve\" data-variant=\"primary\">\
         Approve</button></form>\n\
         <form method=\"post\" action=\"/v1/oauth/authorize\">\
         <input type=\"hidden\" name=\"request\" value=\"{request}\">\
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></form>\n\
         </div>\n</main>\n</body>\n</html>\n"
    )
}

/// An HTML answer with the headers every page in this module carries:
/// never cached, never framed, and the policy the page states.
pub(crate) fn html(status: u16, body: String, csp: &str) -> Response {
    let status = cratefield_core::axum::http::StatusCode::from_u16(status)
        .unwrap_or(cratefield_core::axum::http::StatusCode::OK);
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    if let Ok(value) = HeaderValue::from_str(csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 Appendix B: the one published S256 vector.
    #[test]
    fn s256_matches_the_rfc_7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(verifier_matches(
            verifier,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!verifier_matches(
            "xBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
    }

    #[test]
    fn a_verifier_is_43_to_128_unreserved_characters() {
        assert!(valid_verifier(&"a".repeat(43)));
        assert!(valid_verifier(&"a".repeat(128)));
        assert!(!valid_verifier(&"a".repeat(42)));
        assert!(!valid_verifier(&"a".repeat(129)));
        assert!(!valid_verifier(&format!("{}+", "a".repeat(42))));
        assert!(!valid_verifier(&format!("{}=", "a".repeat(42))));
    }

    /// The registration rule: https anywhere, plain http only to this machine.
    #[test]
    fn redirect_uris_are_https_or_loopback_and_nothing_else() {
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "https://example.test:8443/cb?x=1",
            "http://localhost:3000/callback",
            "http://127.0.0.1/cb",
            "http://[::1]:9000/cb",
        ] {
            assert!(valid_redirect_uri(ok), "{ok} is refused");
        }
        for bad in [
            "http://claude.ai/callback",
            "http://evil.example/cb",
            "javascript:alert(1)",
            "myapp://callback",
            "https://claude.ai/cb#frag",
            "https:///no-host",
            "https://user@claude.ai/cb",
            "https://claude.ai/a b",
            "",
            "claude.ai/cb",
            "http://localhost.evil.example/cb",
        ] {
            assert!(!valid_redirect_uri(bad), "{bad} is accepted");
        }
    }

    #[test]
    fn params_are_appended_with_the_right_separator_and_encoded() {
        assert_eq!(
            with_params("https://c.example/cb", &[("code", "a b"), ("state", "x&y")]),
            "https://c.example/cb?code=a%20b&state=x%26y"
        );
        assert_eq!(
            with_params("https://c.example/cb?z=1", &[("code", "c")]),
            "https://c.example/cb?z=1&code=c"
        );
    }

    #[test]
    fn escaping_covers_the_five_characters() {
        assert_eq!(
            escape("<a href='x'>&\"</a>"),
            "&lt;a href=&#39;x&#39;&gt;&amp;&quot;&lt;/a&gt;"
        );
    }
}
