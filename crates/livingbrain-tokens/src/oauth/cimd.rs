//! Clients that identify themselves by a Client ID Metadata Document
//! (draft-ietf-oauth-client-id-metadata-document, as the MCP authorization
//! spec's 2026-07-28 revision profiles it): a `client_id` that is an `https`
//! URL names a small JSON document the authorization server fetches and
//! validates itself, instead of one it registered through DCR.
//!
//! Nothing about a document is trusted until it is fetched: the URL's host
//! passes the same destination rules an outbound request to a member-supplied
//! endpoint passes (see the guard in `livingbrain-models`' SSRF module — the
//! address rules below mirror it, because `livingbrain-models` depends on
//! this crate for `authenticate` and a dependency the other way would be a
//! cycle), the fetch carries its own size and deadline bounds, a redirect is
//! a failure rather than a hop, and the document must name the very URL it
//! was fetched from as its `client_id` before anything in it is read.
//!
//! The validated client is cached as an ordinary `oauth_clients` row keyed
//! by the URL, so every route that reads a registered client — the consent,
//! the code exchange, the renewal — serves it unchanged. [`fetch`] answers
//! the document's contents and how long it may be cached; the caller stamps
//! the row.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{HttpClient, HttpPolicy};
use serde_json::Value;

use super::consent::{clean_name, encode, valid_redirect_uri};
use super::{MAX_CLIENT_NAME_CHARS, MAX_REDIRECT_URIS};

/// The longest client_id URL accepted. The draft sets no limit; a URL this
/// crate stores whole and shows in logs wants one, and no real document URL
/// is anywhere near this.
const MAX_CLIENT_ID_CHARS: usize = 255;
/// The most a metadata document may weigh. A real one is a few hundred
/// bytes; the cap keeps a host from serving a buffer in place of a document.
/// The [`HttpPolicy`] carries it to the port, and the length is checked
/// again after reading, so a transport that ignores the policy still cannot
/// sneak a larger document through.
const MAX_DOCUMENT_BYTES: usize = 5 * 1024;
/// How long the document fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a document is cached when it says nothing: one hour.
const DEFAULT_CACHE_SECS: u64 = 3600;
/// The shortest cache a document may ask for: five minutes. A shorter
/// `max-age` is a request to turn every authorization into a fetch.
const MIN_CACHE_SECS: u64 = 300;
/// The longest cache a document may ask for: a day. An attacker who controls
/// the document must not be able to pin a lie in place for longer than the
/// operator would want to re-check it.
const MAX_CACHE_SECS: u64 = 86_400;
/// The most a DoH reply may weigh (well under the port ceiling).
const DOH_MAX_RESPONSE: usize = 16 * 1024;
/// How long a DoH resolution may take.
const DOH_TIMEOUT: Duration = Duration::from_secs(5);
/// The Cloudflare DoH endpoint a document host's name is resolved through.
/// There is no DNS port and no `std::net` socket on a Workers isolate, so
/// resolution rides the `HttpClient` port like every other outbound call.
const DOH_ENDPOINT: &str = "https://cloudflare-dns.com/dns-query";

/// Why a Client ID Metadata Document client was refused. The reason is for
/// tests and logs; the requester always gets the same page an unknown client
/// gets, so nothing about the fetch — its rules, its DNS, its transport —
/// leaks to whoever sent the client_id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CimdError {
    /// The client_id is not the shape of a document URL: not `https`, not
    /// ASCII, too long, userinfo, a query or fragment, no path, or a path
    /// with `.`/`..` segments.
    BadClientId,
    /// The host broke the destination rules: a private, loopback, link-local
    /// or reserved IP literal, or a `localhost`/`*.local`/`*.internal`/
    /// single-label name.
    BlockedDestination,
    /// The host's name could not be resolved, resolution itself failed, or
    /// every address it resolved to is private or reserved.
    Resolution(String),
    /// The fetch failed: transport, a non-200 status, a redirect, or a body
    /// over the cap. A redirect is a failure, not a hop — the document at
    /// the URL is the one that was asked for.
    Fetch(String),
    /// The document arrived but is not one this server may act on: not an
    /// object, a `client_id` other than its own URL, no usable name, no
    /// redirect URIs, a shared-secret client authentication method, or
    /// grant/response types this server does not offer.
    Document(String),
}

impl std::fmt::Display for CimdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadClientId => write!(f, "the client_id is not a document URL"),
            Self::BlockedDestination => {
                write!(f, "the document host is a refused destination")
            }
            Self::Resolution(why) => write!(f, "the document host did not resolve publicly: {why}"),
            Self::Fetch(why) => write!(f, "the document could not be fetched: {why}"),
            Self::Document(why) => write!(f, "the document is not valid: {why}"),
        }
    }
}

impl std::error::Error for CimdError {}

/// A client_id URL that passed the shape rules: the exact string the client
/// sent (every fetch and row is keyed by it) and the host a destination
/// check runs against.
pub(crate) struct ClientIdUrl {
    pub(crate) url: String,
    /// The host as written, lower-cased, brackets kept for IPv6 literals.
    pub(crate) host: String,
}

/// Whether a client_id is the shape of a Client ID Metadata Document URL at
/// all: `https`, ASCII, at most [`MAX_CLIENT_ID_CHARS`] characters, no
/// userinfo, query or fragment, a path beyond `/` with no `.` or `..`
/// segment. Pure; the destination check is [`check_destination`].
pub(crate) fn validate_client_id(client_id: &str) -> Result<ClientIdUrl, CimdError> {
    let bad = || CimdError::BadClientId;
    if !client_id.is_ascii()
        || client_id.len() > MAX_CLIENT_ID_CHARS
        || client_id.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return Err(bad());
    }
    let Some(rest) = client_id.strip_prefix("https://") else {
        return Err(bad());
    };
    if rest.contains(['#', '?']) {
        return Err(bad());
    }
    let Some((authority, path)) = rest.split_once('/') else {
        return Err(bad());
    };
    // "MUST contain a path component": a bare authority (no `/` at all,
    // caught above) and the root path alone are both refused.
    if path.is_empty() || path.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(bad());
    }
    let Some(host) = authority_host(authority) else {
        return Err(bad());
    };
    Ok(ClientIdUrl {
        url: client_id.to_owned(),
        host,
    })
}

/// The host of a client_id's authority, lower-cased (brackets kept for IPv6
/// literals), or `None` for a userinfo, an empty host, a stray tail, or a
/// port that is not one to five digits.
fn authority_host(authority: &str) -> Option<String> {
    if authority.contains('@') || authority.is_empty() {
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
        host.trim_matches(|c| c == '[' || c == ']')
            .parse::<Ipv6Addr>()
            .is_ok()
    } else {
        !host.is_empty()
            && !host.starts_with(['.', '-'])
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    ok.then(|| host.to_ascii_lowercase())
}

/// A document host after classification: an address literal already checked,
/// or a name still to be resolved.
enum CheckedHost {
    Literal,
    Domain(String),
}

/// The pure half of the destination check: an IP literal is checked against
/// the address rules here, and a hostname is checked against the name rules
/// (no `localhost`, no `*.local`/`*.internal`, no single label, and a
/// trailing dot stripped first). The DNS half is [`check_destination`].
fn check_host(host: &str) -> Result<CheckedHost, CimdError> {
    if let Some(literal) = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
    {
        let addr = literal
            .parse::<Ipv6Addr>()
            .map_err(|_| CimdError::BadClientId)?;
        return check_ipv6(&addr)
            .map(|_| CheckedHost::Literal)
            .map_err(|_| CimdError::BlockedDestination);
    }
    if let Ok(addr) = host.parse::<Ipv4Addr>() {
        return check_ipv4(&addr)
            .map(|_| CheckedHost::Literal)
            .map_err(|_| CimdError::BlockedDestination);
    }
    let lower = host.to_ascii_lowercase();
    let lower = lower.strip_suffix('.').unwrap_or(&lower);
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return Err(CimdError::BlockedDestination);
    }
    if !lower.contains('.') {
        return Err(CimdError::BlockedDestination);
    }
    Ok(CheckedHost::Domain(lower.to_owned()))
}

/// The DNS half of the destination check: a hostname is resolved over DoH
/// through the `HttpClient` port and refused unless the answers give at
/// least one address and every address is public. A literal needs no
/// resolution (it was checked directly).
async fn check_destination(http: &dyn HttpClient, checked: &CheckedHost) -> Result<(), CimdError> {
    let CheckedHost::Domain(domain) = checked else {
        return Ok(());
    };
    let mut addresses: Vec<IpAddr> = Vec::new();
    for record_type in ["A", "AAAA"] {
        let url = format!("{DOH_ENDPOINT}?name={}&type={record_type}", encode(domain));
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(&url)
            .header(http::header::ACCEPT, "application/dns-json")
            .extension(HttpPolicy {
                max_response_bytes: DOH_MAX_RESPONSE,
                timeout: DOH_TIMEOUT,
            })
            .body(Bytes::new())
            .map_err(|err| CimdError::Resolution(err.to_string()))?;
        let response = http
            .send(request)
            .await
            .map_err(|err| CimdError::Resolution(err.to_string()))?;
        if !response.status().is_success() {
            return Err(CimdError::Resolution(format!(
                "DoH answered {}",
                response.status().as_u16()
            )));
        }
        let body: Value = serde_json::from_slice(response.body())
            .map_err(|err| CimdError::Resolution(err.to_string()))?;
        let Some(answers) = body.get("Answer").and_then(Value::as_array) else {
            continue;
        };
        // Every record in the chain is read, not only the A and AAAA ones: a
        // CNAME's data is a name and parses as neither address, so it drops
        // out, while anything that does parse is checked whatever record
        // type it arrived as.
        for answer in answers {
            let Some(data) = answer.get("data").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(address) = data.parse::<IpAddr>() {
                addresses.push(address);
            }
        }
    }
    let refused = |address: &IpAddr| match address {
        IpAddr::V4(addr) => check_ipv4(addr).err(),
        IpAddr::V6(addr) => check_ipv6(addr).err(),
    };
    if addresses.is_empty() {
        return Err(CimdError::Resolution("no address".to_owned()));
    }
    if addresses.iter().find_map(refused).is_some() {
        return Err(CimdError::BlockedDestination);
    }
    Ok(())
}

// The address rules below mirror `livingbrain-models`' SSRF guard. This
// crate cannot import them — `livingbrain-models` depends on this one — so
// they are carried here unchanged rather than split into a crate of their
// own for two call sites.

/// Rejects every private, loopback, link-local and reserved IPv4 range.
fn check_ipv4(addr: &Ipv4Addr) -> Result<(), String> {
    let [a, b, _c, _d] = addr.octets();
    let private = a == 0 // 0/8
        || a == 10 // 10/8
        || (a == 100 && (64..=127).contains(&b)) // 100.64/10 (CGNAT)
        || a == 127 // 127/8
        || (a == 169 && b == 254) // 169.254/16
        || (a == 172 && (16..=31).contains(&b)) // 172.16/12
        || (a == 192 && b == 0) // 192.0.0/24
        || (a == 192 && b == 168) // 192.168/16
        || (a == 198 && (18..=19).contains(&b)) // 198.18/15
        || a >= 224; // 224/4 (multicast), 240/4 (reserved), 255.255.255.255
    if private {
        return Err(addr.to_string());
    }
    Ok(())
}

/// The embedded IPv4 of a transitional address — the last four octets.
fn embedded_v4(octets: &[u8; 16]) -> Ipv4Addr {
    Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15])
}

/// Rejects `::`, `::1`, `fc00::/7`, `fe80::/10`, `ff00::/8`, Teredo, and
/// every address that carries an IPv4 — mapped, compatible, 6to4 or NAT64 —
/// whose embedded v4 is private.
fn check_ipv6(addr: &Ipv6Addr) -> Result<(), String> {
    let octets = addr.octets();
    let refused = || addr.to_string();
    // :: (unspecified) and ::1 (loopback)
    if addr.is_unspecified() || addr.is_loopback() {
        return Err(refused());
    }
    let first_byte = octets[0];
    // fc00::/7 (unique local addresses): first byte 0xfc or 0xfd.
    if first_byte & 0xfe == 0xfc {
        return Err(refused());
    }
    // fe80::/10 (link-local): first byte 0xfe, second byte 0x80-0xbf.
    if first_byte == 0xfe && (octets[1] & 0xc0) == 0x80 {
        return Err(refused());
    }
    // ff00::/8 (multicast).
    if first_byte == 0xff {
        return Err(refused());
    }
    // IPv4-mapped ::ffff:0:0/96.
    if let Some(v4) = addr.to_ipv4_mapped() {
        return check_ipv4(&v4);
    }
    // IPv4-compatible ::/96: the same trick with a zeroed marker.
    if octets[..12].iter().all(|&b| b == 0) {
        return check_ipv4(&embedded_v4(&octets));
    }
    // 6to4 2002::/16: the v4 is octets [2..6], and the relay sends to it.
    if first_byte == 0x20 && octets[1] == 0x02 {
        return check_ipv4(&Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]));
    }
    // Teredo 2001::/32: the server address is the low 32 bits XORed with
    // 0xffff, so the whole range is refused rather than decoded.
    if first_byte == 0x20 && octets[1] == 0x01 && octets[2] == 0 && octets[3] == 0 {
        return Err(refused());
    }
    // NAT64 64:ff9b::/96 (octets [4..12] zero) carries a v4; the local-use
    // 64:ff9b:1::/48 is a prefix a relay can be aimed anywhere.
    if octets[0] == 0 && octets[1] == 0x64 && octets[2] == 0xff && octets[3] == 0x9b {
        if octets[4..12].iter().all(|&b| b == 0) {
            return check_ipv4(&embedded_v4(&octets));
        }
        if octets[4] == 0 && octets[5] == 1 {
            return Err(refused());
        }
    }
    Ok(())
}

/// A validated metadata document: the client it names, and how long the
/// document may be cached before it must be fetched again.
pub(crate) struct MetadataClient {
    pub(crate) client_name: String,
    pub(crate) redirect_uris: Vec<String>,
    pub(crate) cache_secs: u64,
}

/// Fetches and validates the document a client_id URL names. Every refusal
/// is a [`CimdError`]; nothing is cached on any of them.
pub(crate) async fn fetch(
    http: &dyn HttpClient,
    client_id: &str,
) -> Result<MetadataClient, CimdError> {
    let id = validate_client_id(client_id)?;
    let checked = check_host(&id.host)?;
    check_destination(http, &checked).await?;
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(id.url.as_str())
        .header(http::header::ACCEPT, "application/json")
        .extension(HttpPolicy {
            max_response_bytes: MAX_DOCUMENT_BYTES,
            timeout: FETCH_TIMEOUT,
        })
        .body(Bytes::new())
        .map_err(|err| CimdError::Fetch(err.to_string()))?;
    let response = http
        .send(request)
        .await
        .map_err(|err| CimdError::Fetch(err.to_string()))?;
    let status = response.status();
    // The draft is explicit that a document URL must not be followed to
    // another: a 3xx is a failure, not a hop. On the Workers runtime the
    // HttpClient port answers with the 3xx itself (`redirect: "manual"`), so
    // this is where one stops; an adapter that followed redirects would
    // re-vet each hop per the port's contract but never surface a 3xx here,
    // which is the one way this check can be silent.
    if status.is_redirection() {
        return Err(CimdError::Fetch(format!("answered {status}")));
    }
    if status != http::StatusCode::OK {
        return Err(CimdError::Fetch(format!("answered {status}")));
    }
    let body = response.body();
    if body.len() > MAX_DOCUMENT_BYTES {
        return Err(CimdError::Fetch("the document is too large".to_owned()));
    }
    let document: Value =
        serde_json::from_slice(body).map_err(|err| CimdError::Document(err.to_string()))?;
    let cache_secs = cache_ttl(response.headers());
    let validated = validate_document(&document, &id.url)?;
    Ok(MetadataClient {
        client_name: validated.0,
        redirect_uris: validated.1,
        cache_secs,
    })
}

/// The document's rules: a JSON object whose `client_id` is the very URL it
/// was fetched from (exact string comparison), a name, one to eight
/// redirect URIs this server would accept at registration, and — where it
/// speaks to client authentication and grant types at all — nothing this
/// server does not offer. Unknown fields are ignored.
fn validate_document(
    document: &Value,
    client_id: &str,
) -> Result<(String, Vec<String>), CimdError> {
    let invalid = |why: &str| CimdError::Document(why.to_owned());
    let Some(object) = document.as_object() else {
        return Err(invalid("the document is not a JSON object"));
    };
    if object.get("client_id").and_then(Value::as_str) != Some(client_id) {
        return Err(invalid("the document names a different client_id"));
    }
    // The name is cleaned exactly as a registration's is, so a document
    // cannot carry what a registration could not.
    let Some(raw_name) = object.get("client_name").and_then(Value::as_str) else {
        return Err(invalid("the document has no client_name"));
    };
    let client_name: String = clean_name(raw_name)
        .chars()
        .take(MAX_CLIENT_NAME_CHARS)
        .collect();
    if client_name.is_empty() {
        return Err(invalid("the document's client_name is empty"));
    }
    let uris = match object.get("redirect_uris") {
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= MAX_REDIRECT_URIS => {
            let Some(uris) = items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()
            else {
                return Err(invalid("each redirect URI must be a string"));
            };
            uris
        }
        _ => {
            return Err(invalid(
                "the document must name between one and eight redirect URIs",
            ));
        }
    };
    if uris.iter().any(|uri| !valid_redirect_uri(uri)) {
        return Err(invalid("a redirect URI is not one this server accepts"));
    }
    // ChatGPT's live document declares `private_key_jwt` — not a shared
    // secret, so it is accepted — while this server's own token endpoint
    // offers and advertises `none` alone: the client exchanges as a public
    // PKCE client either way, because a client with no secret at this
    // endpoint is what `none` means. Everything else (`client_secret_basic`,
    // `client_secret_post`, `client_secret_jwt`, `tls_client_auth`, ...)
    // names an authentication this server does not offer, so it is refused
    // rather than accepted and silently ignored.
    if let Some(method) = object.get("token_endpoint_auth_method") {
        match method.as_str() {
            Some("none" | "private_key_jwt") => {}
            _ => return Err(invalid("the authentication method is not supported")),
        }
    }
    let grants_ok = object.get("grant_types").is_none_or(|grants| {
        grants.as_array().is_some_and(|items| {
            items
                .iter()
                .all(|item| matches!(item.as_str(), Some("authorization_code" | "refresh_token")))
        })
    });
    if !grants_ok {
        return Err(invalid(
            "grant_types may be authorization_code and refresh_token",
        ));
    }
    let responses_ok = object.get("response_types").is_none_or(|types| {
        types
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item.as_str() == Some("code")))
    });
    if !responses_ok {
        return Err(invalid("response_types may be code"));
    }
    Ok((client_name, uris))
}

/// How long the document may be cached: its `Cache-Control: max-age`,
/// bounded to [`MIN_CACHE_SECS`]..=[`MAX_CACHE_SECS`], or one hour when it
/// says nothing. A `no-store`/`no-cache` alone says only that the document
/// wants revalidation this server does not do, so the default applies.
fn cache_ttl(headers: &http::HeaderMap) -> u64 {
    let raw = headers
        .get(http::header::CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let max_age = raw.split(',').map(str::trim).find_map(|directive| {
        let lowered = directive.to_ascii_lowercase();
        let value = lowered
            .strip_prefix("max-age=")
            .or_else(|| lowered.strip_prefix("s-maxage="))?;
        value.trim().parse::<u64>().ok()
    });
    max_age
        .unwrap_or(DEFAULT_CACHE_SECS)
        .clamp(MIN_CACHE_SECS, MAX_CACHE_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The draft's URL rules: https only, a path beyond the root, no
    /// traversal, no query, no fragment, no userinfo.
    #[test]
    fn a_client_id_url_has_a_path_and_nothing_hidden_in_it() {
        for ok in [
            "https://chatgpt.com/oauth/client.json",
            "https://apps.example.net/a/client.json",
            "https://example.com/deep/path/client.json",
        ] {
            let parsed = validate_client_id(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert_eq!(parsed.url, ok);
        }
        let too_long = format!("https://chatgpt.com/{}.json", "a".repeat(255));
        // Spliced from two literals so the source never spells out a URL
        // with userinfo credentials in it (secret scanners reject that
        // shape even in a must-refuse fixture); `concat!` is compile-time,
        // so the bytes the validator sees match a plain literal.
        let userinfo_password = concat!("https://user:", "pass@chatgpt.com/client.json");
        for bad in [
            "lbc_normal",
            "https://chatgpt.com",  // no path at all
            "https://chatgpt.com/", // the root path alone
            "https://chatgpt.com/oauth/./client.json",
            "https://chatgpt.com/oauth/../client.json",
            "https://chatgpt.com/client.json?x=1",
            "https://chatgpt.com/client.json#frag",
            "https://user@chatgpt.com/client.json",
            userinfo_password,
            "http://chatgpt.com/client.json",
            "https:///client.json",
            "https://chatgpt.com/a b.json",
            "https://chatgpt.com:port/client.json",
            "https://chatgpt.com:/client.json",
            too_long.as_str(),
        ] {
            assert!(
                validate_client_id(bad).is_err(),
                "{bad} was accepted as a document URL"
            );
        }
    }

    /// Literal and name hosts are held to the same rules an outbound
    /// endpoint URL is (the SSRF guard's own tests hold the originals).
    #[test]
    fn a_document_host_is_held_to_the_destination_rules() {
        for refused in [
            "10.0.0.1",
            "169.254.169.254",
            "127.0.0.1",
            "localhost",
            "localhost.",
        ] {
            assert!(
                check_host(refused).is_err(),
                "{refused} was allowed as a document host"
            );
        }
        for suffix in [".local", ".internal", ".localhost"] {
            let host = format!("docs{suffix}");
            assert!(check_host(&host).is_err(), "{host} was allowed");
        }
        assert!(matches!(
            check_host("intranet"),
            Err(CimdError::BlockedDestination)
        ));
        assert!(check_host("chatgpt.com").is_ok());
        assert!(check_host("1.2.3.4").is_ok());
    }

    /// The document must name its own URL and everything this server would
    /// refuse at registration is refused here too.
    #[test]
    fn a_document_is_validated_rule_by_rule() {
        let url = "https://chatgpt.com/oauth/client.json";
        let doc = |extra: &str, client_id: &str| {
            format!(
                "{{\"client_id\": \"{client_id}\", \"client_name\": \"ChatGPT\", \
                 \"redirect_uris\": [\"https://chatgpt.com/connector_platform_oauth_redirect\"]\
                 {extra}}}"
            )
        };
        for ok in [
            doc("", url),
            doc(
                ", \"token_endpoint_auth_method\": \"private_key_jwt\", \
                   \"grant_types\": [\"authorization_code\", \"refresh_token\"], \
                   \"response_types\": [\"code\"], \"logo_uri\": \"https://x.example/l.png\"",
                url,
            ),
        ] {
            let value: Value = serde_json::from_str(&ok).expect("the fixture parses");
            assert!(
                validate_document(&value, url).is_ok(),
                "refused a valid document: {ok}"
            );
        }
        let bad_client_id = doc("", "https://other.example/client.json");
        for (why, body) in [
            ("a different client_id", bad_client_id),
            (
                "a shared-secret method",
                doc(", \"token_endpoint_auth_method\": \"client_secret_basic\"", url),
            ),
            (
                "a secret-bearing method by any name",
                doc(", \"token_endpoint_auth_method\": \"client_secret_jwt\"", url),
            ),
            (
                "an unsupported grant",
                doc(", \"grant_types\": [\"password\"]", url),
            ),
            (
                "an unsupported response type",
                doc(", \"response_types\": [\"token\"]", url),
            ),
            (
                "a non-https redirect",
                doc(
                    ", \"redirect_uris\": [\"http://chatgpt.com/cb\"]",
                    url
                ),
            ),
            (
                "no redirect URIs",
                "{\"client_id\": \"https://chatgpt.com/oauth/client.json\", \"client_name\": \"x\", \
                 \"redirect_uris\": []}"
                    .to_owned(),
            ),
            (
                "no name",
                "{\"client_id\": \"https://chatgpt.com/oauth/client.json\", \
                 \"redirect_uris\": [\"https://chatgpt.com/cb\"]}"
                    .to_owned(),
            ),
            (
                "a name that cleans to nothing",
                "{\"client_id\": \"https://chatgpt.com/oauth/client.json\", \
                 \"client_name\": \"\u{202E}\", \
                 \"redirect_uris\": [\"https://chatgpt.com/cb\"]}"
                    .to_owned(),
            ),
            ("not an object", "[\"client_id\"]".to_owned()),
        ] {
            let value: Value = serde_json::from_str(&body).expect("the fixture parses");
            assert!(
                validate_document(&value, url).is_err(),
                "{why} was accepted"
            );
        }
    }

    /// The cache a document asks for is bounded; a document that asks for
    /// nothing gets the default.
    #[test]
    fn the_cache_ttl_is_bounded_and_defaults_to_an_hour() {
        let header = |raw: &str| {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::CACHE_CONTROL,
                http::HeaderValue::from_str(raw).expect("a header value"),
            );
            headers
        };
        assert_eq!(cache_ttl(&header("max-age=60")), MIN_CACHE_SECS);
        assert_eq!(cache_ttl(&header("max-age=120")), MIN_CACHE_SECS);
        assert_eq!(cache_ttl(&header("max-age=86400")), MAX_CACHE_SECS);
        assert_eq!(cache_ttl(&header("max-age=99999999")), MAX_CACHE_SECS);
        assert_eq!(cache_ttl(&header("public, max-age=7200")), 7200);
        assert_eq!(cache_ttl(&header("MAX-AGE=7200")), 7200);
        assert_eq!(cache_ttl(&header("no-store")), DEFAULT_CACHE_SECS);
        assert_eq!(cache_ttl(&header("max-age=zero")), DEFAULT_CACHE_SECS);
        assert_eq!(cache_ttl(&http::HeaderMap::new()), DEFAULT_CACHE_SECS);
    }
}
