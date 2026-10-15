//! Where a page lives on the web: the one link every surface cites.
//!
//! Search results, ask citations, note receipts, the page routes, the MCP
//! tools and the Slack agent all hand people a URL for a page. They used to
//! build it against the marketing site (`https://livingbrain.wiki/brain/…`),
//! which serves no pages at all. The pages are read in the web app, which a
//! deployment serves on its **own** origin (`app.livingbrain.wiki` in
//! production, `staging-api.livingbrain.wiki` on staging, the tunnel or
//! loopback under `wrangler dev`) — so the origin is the deployment's
//! config, not a constant.
//!
//! **One setting:** [`APP_BASE_KEY`], the public origin the deployment
//! already declares for the links it mails and redirects to
//! (`WORKSPACES_REDIRECT_BASE`). Unset or malformed, a link falls back to
//! [`DEFAULT_APP_BASE`], the production app, rather than to a relative
//! link nobody can click from a terminal or a chat.
//!
//! The URL is the web app's own page address, `wiki?slug=…` (what
//! `app/assets/wiki.js` loads and pushes into history), plus the page's
//! `scope`: the app opens a page by slug alone, and the web app and the Mac
//! app read the scope back from the link to label and pin a page.

use cratefield_core::Config;

/// The config key the deployment's public origin is read from — the same
/// origin a sign-in link and the Slack redirect are built under.
pub const APP_BASE_KEY: &str = "WORKSPACES_REDIRECT_BASE";

/// The origin a link is built under when [`APP_BASE_KEY`] is unset or is
/// not an absolute http(s) origin: the production web app.
pub const DEFAULT_APP_BASE: &str = "https://app.livingbrain.wiki";

/// The deployment's public web-app origin, without a trailing slash.
///
/// [`APP_BASE_KEY`] when it is set to an absolute `http(s)://` origin,
/// otherwise [`DEFAULT_APP_BASE`]. A blank value reads as unset.
#[must_use]
pub fn app_base(cfg: &dyn Config) -> String {
    cfg.get(APP_BASE_KEY)
        .map(|raw| raw.trim().trim_end_matches('/').to_owned())
        .filter(|base| base.starts_with("https://") || base.starts_with("http://"))
        .filter(|base| !base.contains(['?', '#']))
        .unwrap_or_else(|| DEFAULT_APP_BASE.to_owned())
}

/// The web app's address for one page: `{base}/wiki?slug={slug}&scope={scope}`,
/// both values percent-encoded.
#[must_use]
pub fn page_url(base: &str, scope: &str, slug: &str) -> String {
    format!(
        "{}/wiki?slug={}&scope={}",
        base.trim_end_matches('/'),
        encode(slug),
        encode(scope),
    )
}

/// [`page_url`] for a `scope/slug` reference, split on its first `/` (a
/// scope never holds one). A reference with no `/` is read as a bare slug.
#[must_use]
pub fn reference_url(base: &str, reference: &str) -> String {
    match reference.split_once('/') {
        Some((scope, slug)) => page_url(base, scope, slug),
        None => page_url(base, "", reference),
    }
}

/// `encodeURIComponent`'s rule: every byte but the unreserved set
/// (`A–Z a–z 0–9 - _ . ~`) as `%XX`.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Map(HashMap<&'static str, &'static str>);

    impl Config for Map {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).map(|value| (*value).to_owned())
        }
    }

    fn cfg(pairs: &[(&'static str, &'static str)]) -> Map {
        Map(pairs.iter().copied().collect())
    }

    #[test]
    fn the_origin_is_the_deployments_own() {
        for (configured, expected) in [
            (
                "https://app.livingbrain.wiki",
                "https://app.livingbrain.wiki",
            ),
            (
                "https://staging-api.livingbrain.wiki/",
                "https://staging-api.livingbrain.wiki",
            ),
            ("http://localhost:8787", "http://localhost:8787"),
        ] {
            assert_eq!(app_base(&cfg(&[(APP_BASE_KEY, configured)])), expected);
        }
    }

    #[test]
    fn an_unset_or_malformed_origin_falls_back_to_the_app() {
        for bad in [
            "",
            "  ",
            "app.livingbrain.wiki",
            "ftp://x",
            "https://x/?a=b",
        ] {
            assert_eq!(app_base(&cfg(&[(APP_BASE_KEY, bad)])), DEFAULT_APP_BASE);
        }
        assert_eq!(app_base(&cfg(&[])), DEFAULT_APP_BASE);
    }

    #[test]
    fn a_page_url_is_the_web_apps_page_address() {
        assert_eq!(
            page_url("https://app.livingbrain.wiki", "user-01HX", "q3-plan"),
            "https://app.livingbrain.wiki/wiki?slug=q3-plan&scope=user-01HX"
        );
        assert_eq!(
            page_url("https://x.test/", "shared", "coding-agents/repo/a b"),
            "https://x.test/wiki?slug=coding-agents%2Frepo%2Fa%20b&scope=shared"
        );
        assert_eq!(
            reference_url("https://x.test", "shared/kestrel"),
            "https://x.test/wiki?slug=kestrel&scope=shared"
        );
    }

    #[test]
    fn no_link_points_at_the_marketing_site() {
        let url = page_url(&app_base(&cfg(&[])), "shared", "kestrel");
        assert!(!url.starts_with("https://livingbrain.wiki"), "{url}");
        assert!(!url.contains("/brain/"), "{url}");
    }
}
