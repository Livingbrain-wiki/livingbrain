//! The SSRF guard: validate a caller-supplied URL before any request is
//! made to it.
//!
//! A custom endpoint URL is the one place a member can point this module
//! at an address the server will fetch, so the guard runs before the probe
//! and the checked, normalized [`Url`] is what the probe then uses. It is
//! pure URL and address validation plus DNS-over-HTTPS resolution through
//! the `HttpClient` port (there is no DNS port and no `std::net` socket on
//! a Workers isolate). A literal IP is checked directly; a hostname is
//! checked against what DNS answers, not against its name, and a
//! resolution that yields no address at all is a refusal rather than a
//! pass.
//!
//! # What this does not cover
//!
//! Two holes are known and are **not** closed here:
//!
//! - **Redirects.** `HttpPolicy` carries no redirect setting, so the port
//!   gives no way to say "do not follow". The Workers adapter
//!   (`cratefield_runtime_cloudflare::FetchClient`) follows them by default
//!   and never produces `HttpError::BlockedDestination`, so an endpoint
//!   that answers `302` to `169.254.169.254` is fetched. Closing this needs
//!   the port to carry a redirect mode.
//! - **DNS rebinding.** The name is resolved once, through DoH, and the
//!   runtime resolves it again when it connects. An answer that is public
//!   at the first lookup and private at the second passes this guard.
//!
//! Both are tracked separately. Until then the guard is what it says it
//! is: a check on the URL and on the addresses the name resolves to.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{HttpClient, HttpError, HttpPolicy};
use url::{Host, Url};

/// The maximum response body for a DoH reply (well under the port ceiling).
const DOH_MAX_RESPONSE: usize = 64 * 1024;
/// The timeout for a DoH resolution.
const DOH_TIMEOUT: Duration = Duration::from_secs(10);
/// The Cloudflare DoH endpoint used to resolve hostnames.
const DOH_ENDPOINT: &str = "https://cloudflare-dns.com/dns-query";

/// An SSRF refusal: which rule the URL broke. The message names the rule,
/// never the provider key — and never echoes the URL back, so nothing a
/// member typed leaves the server through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SsrfError {
    /// The URL did not parse at all.
    NotAUrl,
    /// The scheme was not `https`.
    Scheme(String),
    /// The URL carried userinfo (`user:pass@`).
    Userinfo,
    /// The host was `localhost`, `*.localhost`, `*.local` or `*.internal`.
    LocalhostHost,
    /// The host was a single label (no dot, not an IP literal).
    SingleLabelHost,
    /// An address is private, loopback, link-local or reserved — whether
    /// it was written into the URL as a literal or came back from DNS.
    PrivateIp(String),
    /// A hostname resolved to nothing.
    UnresolvedHost,
    /// The DoH request itself failed (transport, non-2xx, parse).
    DohFailed(String),
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAUrl => write!(f, "the endpoint URL could not be parsed"),
            Self::Scheme(s) => write!(f, "scheme must be https, got {s:?}"),
            Self::Userinfo => write!(f, "userinfo (user:pass@) is not allowed"),
            Self::LocalhostHost => {
                write!(
                    f,
                    "localhost, *.localhost, *.local and *.internal hosts are not allowed"
                )
            }
            Self::SingleLabelHost => write!(f, "single-label hosts are not allowed"),
            Self::PrivateIp(ip) => write!(f, "address {ip} is private, loopback or reserved"),
            Self::UnresolvedHost => write!(f, "host could not be resolved"),
            Self::DohFailed(msg) => write!(f, "DNS resolution failed: {msg}"),
        }
    }
}

impl std::error::Error for SsrfError {}

/// Validates the URL's structure: scheme, userinfo, host name. Returns the
/// parsed `Url` for further address checking.
///
/// Pure: no I/O. The DNS resolution for hostname hosts is
/// [`check_destination`].
pub(crate) fn validate_url(raw: &str) -> Result<Url, SsrfError> {
    let url = Url::parse(raw).map_err(|_| SsrfError::NotAUrl)?;
    if url.scheme() != "https" {
        return Err(SsrfError::Scheme(url.scheme().to_owned()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(SsrfError::Userinfo);
    }
    let Some(host) = url.host_str() else {
        return Err(SsrfError::SingleLabelHost);
    };
    match url.host() {
        Some(Host::Ipv4(addr)) => {
            check_ipv4(&addr).map_err(|_| SsrfError::PrivateIp(host.to_owned()))
        }
        Some(Host::Ipv6(addr)) => {
            check_ipv6(&addr).map_err(|_| SsrfError::PrivateIp(host.to_owned()))
        }
        Some(Host::Domain(domain)) => check_hostname(domain),
        None => Err(SsrfError::SingleLabelHost),
    }?;
    Ok(url)
}

/// Checks the destination's addresses, resolving hostnames via DoH.
/// Call after [`validate_url`].
pub(crate) async fn check_destination(http: &dyn HttpClient, url: &Url) -> Result<(), SsrfError> {
    match url.host() {
        Some(Host::Ipv4(addr)) => check_ipv4(&addr),
        Some(Host::Ipv6(addr)) => check_ipv6(&addr),
        Some(Host::Domain(domain)) => resolve_and_check(http, domain).await,
        None => Err(SsrfError::SingleLabelHost),
    }
}

/// Rejects `localhost`, `*.localhost`, `*.local`, `*.internal`, and
/// single-label hosts.
///
/// One trailing dot is stripped first: `localhost.` and
/// `models.internal.` name the same hosts as their undotted spellings and
/// resolve the same way, so a rule that missed them would be a hole rather
/// than a strictness.
fn check_hostname(domain: &str) -> Result<(), SsrfError> {
    let lower = domain.to_ascii_lowercase();
    let lower = lower.strip_suffix('.').unwrap_or(&lower);
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err(SsrfError::LocalhostHost);
    }
    if lower.ends_with(".local") || lower.ends_with(".internal") {
        return Err(SsrfError::LocalhostHost);
    }
    if !lower.contains('.') {
        return Err(SsrfError::SingleLabelHost);
    }
    Ok(())
}

/// Rejects every private, loopback, link-local and reserved IPv4 range.
fn check_ipv4(addr: &Ipv4Addr) -> Result<(), SsrfError> {
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
        return Err(SsrfError::PrivateIp(addr.to_string()));
    }
    Ok(())
}

/// The embedded IPv4 of a transitional address — the last four octets.
fn embedded_v4(octets: &[u8; 16]) -> Ipv4Addr {
    Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15])
}

/// Rejects `::`, `::1`, `fc00::/7`, `fe80::/10`, `ff00::/8`, Teredo, and
/// every address that carries an IPv4 — mapped, compatible, 6to4 or
/// NAT64 — whose embedded v4 is private.
fn check_ipv6(addr: &Ipv6Addr) -> Result<(), SsrfError> {
    let octets = addr.octets();
    let refused = || SsrfError::PrivateIp(addr.to_string());
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
    // NAT64 64:ff9b::/96 (octets [4..12] zero) carries a v4; the
    // local-use 64:ff9b:1::/48 is a prefix a relay can be aimed anywhere.
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

/// Resolves `domain` via DNS-over-HTTPS and refuses unless the answers
/// give at least one address and every address is public.
///
/// Only records that parse as an address count. A reply that carries only
/// CNAMEs, or only a TXT record, resolves to no address at all — counting
/// its strings as if they were addresses would let a name that resolves to
/// nothing through as a name that resolves to somewhere public, which is
/// the fail-open this refuses.
async fn resolve_and_check(http: &dyn HttpClient, domain: &str) -> Result<(), SsrfError> {
    let mut addresses: Vec<IpAddr> = Vec::new();
    for record_type in ["A", "AAAA"] {
        let url = format!(
            "{DOH_ENDPOINT}?name={}&type={record_type}",
            encode_domain(domain)
        );
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(&url)
            .header(http::header::ACCEPT, "application/dns-json")
            .extension(HttpPolicy {
                max_response_bytes: DOH_MAX_RESPONSE,
                timeout: DOH_TIMEOUT,
            })
            .body(Bytes::new())
            .map_err(|e| SsrfError::DohFailed(e.to_string()))?;
        let response = http.send(request).await.map_err(|e| match e {
            HttpError::BlockedDestination(msg) => SsrfError::DohFailed(format!("blocked: {msg}")),
            other => SsrfError::DohFailed(other.to_string()),
        })?;
        if !response.status().is_success() {
            return Err(SsrfError::DohFailed(format!(
                "DoH returned status {}",
                response.status().as_u16()
            )));
        }
        let body: serde_json::Value = serde_json::from_slice(response.body())
            .map_err(|e| SsrfError::DohFailed(e.to_string()))?;
        let Some(answers) = body.get("Answer").and_then(serde_json::Value::as_array) else {
            continue;
        };
        // Every record in the chain is read, not only the A and AAAA ones:
        // a CNAME's data is a name and parses as neither address, so it
        // drops out, while anything that *does* parse is checked whatever
        // record type it arrived as.
        for answer in answers {
            let Some(data) = answer.get("data").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Ok(address) = data.parse::<IpAddr>() {
                addresses.push(address);
            }
        }
    }
    let Some(first) = addresses.first() else {
        return Err(SsrfError::UnresolvedHost);
    };
    match first {
        IpAddr::V4(addr) => check_ipv4(addr)?,
        IpAddr::V6(addr) => check_ipv6(addr)?,
    }
    for address in &addresses[1..] {
        match address {
            IpAddr::V4(addr) => check_ipv4(addr)?,
            IpAddr::V6(addr) => check_ipv6(addr)?,
        }
    }
    Ok(())
}

/// Percent-encodes a domain name for the DoH query string.
fn encode_domain(domain: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(domain, UNRESERVED).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_is_rejected() {
        assert!(matches!(
            validate_url("http://api.example.com/v1"),
            Err(SsrfError::Scheme(_))
        ));
    }

    #[test]
    fn https_with_10_is_rejected() {
        assert!(matches!(
            validate_url("https://10.1.2.3/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn https_with_metadata_is_rejected() {
        assert!(matches!(
            validate_url("https://169.254.169.254/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn loopback_ipv6_is_rejected() {
        assert!(matches!(
            validate_url("https://[::1]/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn link_local_ipv6_is_rejected() {
        assert!(matches!(
            validate_url("https://[fe80::1]/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn ipv4_mapped_ipv6_is_rejected() {
        assert!(matches!(
            validate_url("https://[::ffff:10.0.0.1]/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn userinfo_is_rejected() {
        assert!(matches!(
            validate_url("https://user:pass@api.example.com/v1"),
            Err(SsrfError::Userinfo)
        ));
    }

    #[test]
    fn localhost_is_rejected() {
        assert!(matches!(
            validate_url("https://localhost/v1"),
            Err(SsrfError::LocalhostHost)
        ));
    }

    #[test]
    fn single_label_host_is_rejected() {
        assert!(matches!(
            validate_url("https://internal/v1"),
            Err(SsrfError::SingleLabelHost)
        ));
    }

    #[test]
    fn an_internal_suffix_is_rejected() {
        assert!(matches!(
            validate_url("https://models.internal/v1"),
            Err(SsrfError::LocalhostHost)
        ));
    }

    #[test]
    fn public_ip_is_accepted() {
        assert!(validate_url("https://1.2.3.4/v1").is_ok());
    }

    #[test]
    fn public_domain_is_accepted() {
        assert!(validate_url("https://api.openai.com/v1").is_ok());
    }

    #[test]
    fn cgnat_is_rejected() {
        assert!(matches!(
            validate_url("https://100.64.0.1/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn benchmarking_range_is_rejected() {
        assert!(matches!(
            validate_url("https://198.18.0.1/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn unique_local_ipv6_is_rejected() {
        assert!(matches!(
            validate_url("https://[fc00::1]/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    #[test]
    fn multicast_ipv6_is_rejected() {
        assert!(matches!(
            validate_url("https://[ff02::1]/v1"),
            Err(SsrfError::PrivateIp(_))
        ));
    }

    /// `::a.b.c.d`, `2002::/16` and `64:ff9b::/96` each carry an IPv4
    /// address in the low 32 bits, so an unwary check sees only a global
    /// IPv6 range and lets a private v4 through.
    #[test]
    fn ipv6_wrapping_a_private_v4_is_rejected() {
        for url in [
            "https://[::10.0.0.1]/v1",        // IPv4-compatible
            "https://[2002:a00:1::]/v1",      // 6to4
            "https://[64:ff9b::10.0.0.1]/v1", // NAT64 well-known prefix
        ] {
            assert!(
                matches!(validate_url(url), Err(SsrfError::PrivateIp(_))),
                "{url} was accepted"
            );
        }
    }

    /// Teredo and the local-use NAT64 prefix carry no address the guard
    /// can read safely, so the whole range is refused.
    #[test]
    fn teredo_and_local_nat64_are_rejected() {
        for url in ["https://[2001::1]/v1", "https://[64:ff9b:1::1]/v1"] {
            assert!(
                matches!(validate_url(url), Err(SsrfError::PrivateIp(_))),
                "{url} was accepted"
            );
        }
    }

    /// `localhost.` and `x.internal.` resolve exactly as their undotted
    /// spellings do, so the hostname rules have to strip the dot first.
    #[test]
    fn a_trailing_dot_does_not_hide_the_hostname_rules() {
        assert!(matches!(
            validate_url("https://localhost./v1"),
            Err(SsrfError::LocalhostHost)
        ));
        assert!(matches!(
            validate_url("https://x.internal./v1"),
            Err(SsrfError::LocalhostHost)
        ));
        assert!(matches!(
            validate_url("https://intranet./v1"),
            Err(SsrfError::SingleLabelHost)
        ));
    }
}
