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

/// A redirect URI the client may return to: ASCII, absolute, without a
/// fragment, a plain authority (see [`redirect_host`]), and either `https`, or
/// `http` to this machine (`localhost`, `127.0.0.1`, `[::1]`). Anything else
/// is refused at registration, so a code can only ever be sent to an address
/// the client registered as its own.
pub(crate) fn valid_redirect_uri(uri: &str) -> bool {
    if !uri.is_ascii()
        || uri.len() > 512
        || uri.contains('#')
        || uri.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return false;
    }
    let Some((scheme, _)) = uri.split_once("://") else {
        return false;
    };
    let Some(host) = redirect_host(uri) else {
        return false;
    };
    match scheme {
        "https" => true,
        "http" => matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]"),
        _ => false,
    }
}

/// The host of a redirect URI, without its port, lower-cased. `None` when the
/// URI has no acceptable authority: a userinfo, a host outside the plain DNS
/// shape or `[::1]`, or anything after the host that is not `:` and a port of
/// one to five digits.
pub(crate) fn redirect_host(uri: &str) -> Option<String> {
    let rest = uri.split_once("://")?.1;
    let authority = rest.split(['/', '?']).next()?;
    if authority.contains('@') {
        return None;
    }
    let (host, tail) = if let Some(after) = authority.strip_prefix('[') {
        let (inside, tail) = after.split_once(']')?;
        (format!("[{inside}]"), tail.to_owned())
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), format!(":{port}")),
            None => (authority.to_owned(), String::new()),
        }
    };
    if !tail.is_empty() {
        let port = tail.strip_prefix(':')?;
        if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let ok = if host.starts_with('[') {
        host == "[::1]"
    } else {
        !host.is_empty()
            && !host.starts_with(['.', '-'])
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    ok.then(|| host.to_ascii_lowercase())
}

/// Whether a redirect host is one Living Brain recognises as a Claude app:
/// `claude.ai` and `claude.com`, and their subdomains. Only these get the
/// "Claude connector" label; every other app is shown as unverified.
pub(crate) fn verified_host(host: &str) -> bool {
    ["claude.ai", "claude.com"]
        .iter()
        .any(|base| host == *base || host.ends_with(&format!(".{base}")))
}

/// Strips the characters a name must never carry into a settings row or a
/// consent page: control characters (Unicode `Cc`) and the format characters
/// (`Cf`) that reorder or hide text — the bidirectional embeddings and
/// isolates U+202A–U+202E and U+2066–U+2069 most of all, which let a
/// registered name display as something else.
pub(crate) fn clean_name(raw: &str) -> String {
    raw.chars()
        .filter(|&c| !c.is_control() && !is_format(c))
        .collect::<String>()
        .trim()
        .to_owned()
}

fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
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
/// the cleaned, registered one, the redirect host is validated, and `request`
/// is the sealed consent token the form posts back.
pub(crate) struct Consent<'a> {
    pub(crate) client_name: &'a str,
    /// The host the approval sends the person back to, shown in full.
    pub(crate) host: &'a str,
    /// Whether that host is one Living Brain recognises (see [`verified_host`]).
    pub(crate) verified: bool,
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

/// The consent page. Plain English, in the device page's own vocabulary, and
/// it says exactly what the grant does. The return host is shown in full, and
/// an app whose host is not one Living Brain recognises is marked unverified.
pub(crate) fn consent_page(consent: &Consent<'_>) -> String {
    let client = escape(consent.client_name);
    let host = escape(consent.host);
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
    let warning = if consent.verified {
        String::new()
    } else {
        format!(
            "<p class=\"notice\" role=\"alert\"><strong>Unverified app — check this \
             address.</strong> Living Brain does not recognise {client} as an app it has \
             checked. Approve only if you are connecting {host} yourself.</p>"
        )
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <title>Connect {client} to Living Brain</title>\n\
         <link rel=\"stylesheet\" href=\"/assets/app.css\">\n\
         <link rel=\"stylesheet\" href=\"/assets/device.css\">\n</head>\n\
         <body>\n<main class=\"device\">\n\
         <p class=\"eyebrow\">Connect an app</p>\n\
         <h1 id=\"oauth-title\">Let {client} use your Living Brain?</h1>\n\
         <p class=\"lede\">{client} will be able to search and read the pages you can read \
         in this workspace, read facts and sources, and add notes to your own private memory. \
         It cannot change settings, manage tokens, or see other members.</p>\n\
         {warning}\
         <dl class=\"facts device__facts\">\
         <div>{picker}</div>\
         <div><dt>Returns to</dt><dd><strong id=\"oauth-host\"><code>{host}</code></strong></dd></div></dl>\n\
         <p class=\"muted\">You can disconnect at any time in Settings → API tokens. If you \
         did not start this from {client}, deny it.</p>\n\
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
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
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

#[cfg(test)]
mod hardening_tests {
    use super::*;

    /// L1: the authority is a plain host, ASCII, no userinfo, no stray port.
    #[test]
    fn the_authority_is_a_plain_host_and_nothing_that_could_read_as_another() {
        for bad in [
            "https://claude.ai;evil.example/cb",
            "https://claude.ai,evil.example/cb",
            "https://claüde.ai/cb",
            "https://claude.ai:99999999/cb",
            "https://claude.ai:port/cb",
            "https://claude.ai:/cb",
            "https://-claude.ai/cb",
            "https://claude..ai/x y",
            "https://[::1]evil/cb",
        ] {
            assert!(!valid_redirect_uri(bad), "{bad} is accepted");
        }
        assert!(valid_redirect_uri("https://claude.ai:8443/cb"));
        assert_eq!(
            redirect_host("https://Claude.AI:443/cb").as_deref(),
            Some("claude.ai")
        );
    }

    #[test]
    fn only_claude_hosts_are_verified_and_their_subdomains() {
        assert!(verified_host("claude.ai"));
        assert!(verified_host("claude.com"));
        assert!(verified_host("app.claude.ai"));
        assert!(!verified_host("claude.ai.evil.example"));
        assert!(!verified_host("evilclaude.ai"));
        assert!(!verified_host("example.test"));
    }

    /// M1: control and format characters, including the bidi overrides, are
    /// removed; ordinary text is untouched.
    #[test]
    fn a_name_loses_its_control_and_override_characters() {
        assert_eq!(clean_name("\u{202E}enilnoM\u{202C}"), "enilnoM");
        assert_eq!(clean_name("Cl\u{2066}au\u{2069}de\u{200B}"), "Claude");
        assert_eq!(clean_name("tab\there\u{0007}"), "tabhere");
        assert_eq!(clean_name("  Ünïcode ok  "), "Ünïcode ok");
    }
}
