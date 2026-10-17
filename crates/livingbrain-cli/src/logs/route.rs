//! Which workspace a session's upload lands in.
//!
//! One Mac can sync sessions from several repositories into several
//! workspaces. Each allowed root may name its workspace (`logs workspace`),
//! and `routes` in the opt-in file send the rest by the repository's remote
//! owner or folder name. The order is fixed: an explicit root workspace
//! first, then the first matching route, then the default workspace.
//!
//! Everything here is local. The only outside process is `git`, read-only,
//! and only when a route needs the repository's facts.

use serde::{Deserialize, Serialize};

/// The workspace a session goes to when nothing else says: the token
/// `livingbrain login` made. Also the spelling `logs workspace <path> default`
/// stores to mean "the default, even when a route would pick another".
pub const DEFAULT: &str = "default";

/// The longest workspace id this CLI will store or send.
const MAX_TARGET_BYTES: usize = 128;

/// One automatic rule: sessions whose repository matches go to `workspace`.
/// Exactly one of `remote_owner` (the GitHub owner of `origin`) or
/// `folder_glob` (a shell-style pattern over the repository's folder name)
/// is set. An unknown key is refused rather than ignored: a misspelled
/// matcher would otherwise send that repository's sessions to the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_glob: Option<String>,
    pub workspace: String,
}

impl Route {
    /// Whether the route is well formed.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.remote_owner, &self.folder_glob) {
            (Some(owner), None) if !owner.is_empty() => {}
            (None, Some(glob)) if !glob.is_empty() => {}
            _ => {
                return Err(
                    "a route names exactly one non-empty matcher: remote_owner or folder_glob"
                        .to_owned(),
                );
            }
        }
        check_target(&self.workspace)
    }

    fn matches(&self, facts: &RepoFacts) -> bool {
        if let Some(owner) = &self.remote_owner {
            return facts
                .remote_owner
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(owner));
        }
        match (&self.folder_glob, &facts.folder) {
            (Some(glob), Some(folder)) => glob_match(glob, folder),
            _ => false,
        }
    }
}

/// What a route can see about one repository. A fact that cannot be read is
/// `None`, and a route on a missing fact never matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoFacts {
    /// The repository's folder name: the basename of its git top level, or of
    /// the working directory when git does not know it.
    pub folder: Option<String>,
    /// The GitHub owner of `origin`, when it is a github.com remote.
    pub remote_owner: Option<String>,
}

impl RepoFacts {
    /// Read the facts of the repository `cwd` is in. Runs `git` read-only;
    /// every failure just leaves a fact out.
    pub fn probe(cwd: &str) -> Self {
        let top = git(cwd, &["rev-parse", "--show-toplevel"]);
        let folder = top
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| cwd.trim_end_matches('/').to_owned());
        let folder = folder
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .map(str::to_owned);
        let remote_owner = git(cwd, &["remote", "get-url", "origin"])
            .as_deref()
            .and_then(|url| github_owner(url.trim()));
        Self {
            folder,
            remote_owner,
        }
    }
}

/// Run `git -C <cwd> <args>` and return its stdout, or `None` on any failure.
fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The GitHub owner of a remote URL, for `https://`, `ssh://` and the scp
/// form `git@github.com:owner/repo.git`. `None` for any other host or a URL
/// with no owner segment.
pub fn github_owner(url: &str) -> Option<String> {
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        // The authority is up to the first `/`; userinfo ends at its last `@`.
        let (authority, path) = rest.split_once('/')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        // A port is not part of the host name.
        let host = host.split_once(':').map_or(host, |(host, _)| host);
        (host, path)
    } else {
        // scp-like: `[user@]host:path`, with no scheme.
        let (authority, path) = url.split_once(':')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host, path)
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let owner = path.trim_start_matches('/').split('/').next()?;
    (!owner.is_empty()).then(|| owner.to_owned())
}

/// Shell-style matching of a whole name: `*` is any run of characters, `?`
/// any one character, everything else is itself. Case-sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    // Classic two-pointer match with backtracking to the last `*`.
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Whether `id` may name a workspace (or be the word `default`): non-empty,
/// at most [`MAX_TARGET_BYTES`], and only ASCII letters, digits, `-`, `_`, `.`.
pub fn check_target(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= MAX_TARGET_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{id}` is not a workspace id (letters, digits, `-`, `_`, `.`; at most {MAX_TARGET_BYTES} characters)"
        ))
    }
}

/// The workspace of the first route that matches, if any.
pub fn first_match<'a>(routes: &'a [Route], facts: &RepoFacts) -> Option<&'a str> {
    routes
        .iter()
        .find(|route| route.matches(facts))
        .map(|route| route.workspace.as_str())
}

/// Where one session goes, after every working directory it recorded has been
/// resolved. A session is one upload, so all of its directories must agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The default workspace (`login`'s token).
    Default,
    /// A named workspace; its token is read for this target alone.
    Workspace(String),
    /// Its directories resolve to different workspaces. Sent nowhere.
    Mixed,
}

impl Destination {
    /// The word the report and the per-workspace groups use: `default`, the
    /// workspace id, or `mixed`.
    pub fn key(&self) -> &str {
        match self {
            Self::Default => DEFAULT,
            Self::Workspace(id) => id,
            Self::Mixed => "mixed",
        }
    }

    /// The destination of a target as stored: `default` or a workspace id.
    pub fn of_target(target: &str) -> Self {
        if target == DEFAULT {
            Self::Default
        } else {
            Self::Workspace(target.to_owned())
        }
    }

    /// Combine the destinations of a session's directories: all the same, that
    /// one; any disagreement, [`Destination::Mixed`]. No directory means the
    /// default.
    pub fn combine(mut destinations: impl Iterator<Item = Self>) -> Self {
        let Some(first) = destinations.next() else {
            return Self::Default;
        };
        if destinations.all(|next| next == first) {
            first
        } else {
            Self::Mixed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Destination, RepoFacts, Route, check_target, first_match, github_owner, glob_match,
    };

    fn route(matcher: (&str, &str), workspace: &str) -> Route {
        let (key, value) = matcher;
        Route {
            remote_owner: (key == "remote_owner").then(|| value.to_owned()),
            folder_glob: (key == "folder_glob").then(|| value.to_owned()),
            workspace: workspace.to_owned(),
        }
    }

    /// Both URL forms GitHub remotes come in, with and without a `.git` suffix,
    /// userinfo, a port, and the uppercase host some tools write.
    #[test]
    fn a_github_remote_names_its_owner_in_every_form() {
        for url in [
            "https://github.com/CHI-Ecosystem/chi-web.git",
            "https://github.com/CHI-Ecosystem/chi-web",
            "https://x-access-token@github.com/CHI-Ecosystem/chi-web.git",
            "https://GitHub.com:443/CHI-Ecosystem/chi-web/",
            "ssh://git@github.com/CHI-Ecosystem/chi-web.git",
            "ssh://git@github.com:22/CHI-Ecosystem/chi-web.git",
            "git@github.com:CHI-Ecosystem/chi-web.git",
            "github.com:CHI-Ecosystem/chi-web",
        ] {
            assert_eq!(github_owner(url).as_deref(), Some("CHI-Ecosystem"), "{url}");
        }
    }

    /// Other hosts, a bare host and a malformed URL name no owner.
    #[test]
    fn a_remote_on_another_host_names_no_owner() {
        for url in [
            "https://gitlab.com/CHI-Ecosystem/chi-web.git",
            "git@gitlab.example.com:CHI-Ecosystem/chi-web.git",
            "https://github.com.evil.example/CHI-Ecosystem/x",
            "https://github.com/",
            "git@github.com:",
            "/srv/git/local.git",
            "",
        ] {
            assert_eq!(github_owner(url), None, "{url}");
        }
    }

    #[test]
    fn globs_match_whole_names_only() {
        assert!(glob_match("chi-*", "chi-web"));
        assert!(glob_match("chi-*", "chi-"));
        assert!(!glob_match("chi-*", "xchi-web"));
        assert!(!glob_match("chi-*", "chi"));
        assert!(glob_match("c?i-*-app", "chi-x-app"));
        assert!(!glob_match("c?i", "chii"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*web*", "chi-web-pwa"));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("a*b*c", "aXbYcZ"));
        assert!(glob_match("CHI-*", "CHI-web"));
        assert!(!glob_match("CHI-*", "chi-web"), "globs are case-sensitive");
    }

    #[test]
    fn workspace_ids_are_plain_and_default_is_its_own_word() {
        assert!(check_target("ws-chi_1.2").is_ok());
        assert!(check_target("default").is_ok(), "the explicit default");
        for bad in ["", "has space", "a/b", "a#b", "émoji", &"x".repeat(129)] {
            assert!(check_target(bad).is_err(), "{bad}");
        }
    }

    /// A route names exactly one matcher and a valid workspace; a route with
    /// both, neither, or an empty matcher is refused.
    #[test]
    fn a_route_names_exactly_one_matcher() {
        assert!(
            route(("remote_owner", "CHI-Ecosystem"), "ws")
                .validate()
                .is_ok()
        );
        assert!(route(("folder_glob", "chi-*"), "ws").validate().is_ok());
        assert!(route(("folder_glob", "chi-*"), "ws ws").validate().is_err());
        assert!(route(("folder_glob", ""), "ws").validate().is_err());
        let both = Route {
            remote_owner: Some("CHI-Ecosystem".into()),
            folder_glob: Some("chi-*".into()),
            workspace: "ws".into(),
        };
        assert!(both.validate().is_err());
        let neither = Route {
            remote_owner: None,
            folder_glob: None,
            workspace: "ws".into(),
        };
        assert!(neither.validate().is_err());
    }

    /// The first matching route decides; a route whose fact is unknown never
    /// matches, and so falls through to the next.
    #[test]
    fn the_first_matching_route_wins_and_unknown_facts_match_nothing() {
        let routes = vec![
            route(("remote_owner", "CHI-Ecosystem"), "ws-chi"),
            route(("folder_glob", "chi-*"), "ws-folder"),
            route(("folder_glob", "*"), "ws-catch-all"),
        ];
        let owned = RepoFacts {
            folder: Some("chi-web".into()),
            remote_owner: Some("chi-ecosystem".into()),
        };
        assert_eq!(
            first_match(&routes, &owned),
            Some("ws-chi"),
            "owner match is case-insensitive"
        );

        let folder_only = RepoFacts {
            folder: Some("chi-web".into()),
            remote_owner: None,
        };
        assert_eq!(first_match(&routes, &folder_only), Some("ws-folder"));

        let stranger = RepoFacts {
            folder: Some("notes".into()),
            remote_owner: Some("someone".into()),
        };
        assert_eq!(first_match(&routes, &stranger), Some("ws-catch-all"));

        let unreadable = RepoFacts::default();
        assert_eq!(first_match(&routes[..2], &unreadable), None);
    }

    /// Every directory of a session must agree; any disagreement is mixed.
    #[test]
    fn a_session_resolves_to_one_destination_or_is_mixed() {
        let ws = |id: &str| Destination::Workspace(id.to_owned());
        assert_eq!(
            Destination::combine([ws("a"), ws("a")].into_iter()),
            ws("a")
        );
        assert_eq!(
            Destination::combine([ws("a"), Destination::Default].into_iter()),
            Destination::Mixed
        );
        assert_eq!(
            Destination::combine([ws("a"), ws("b")].into_iter()),
            Destination::Mixed
        );
        assert_eq!(
            Destination::combine(std::iter::empty()),
            Destination::Default
        );
        assert_eq!(Destination::of_target("default"), Destination::Default);
        assert_eq!(Destination::of_target("ws-chi").key(), "ws-chi");
    }

    /// The probe of a path that is not a repository still names a folder from
    /// the path, and finds no remote: a route on the remote cannot match it.
    #[test]
    fn a_path_without_git_has_a_folder_and_no_owner() {
        let facts = RepoFacts::probe("/nonexistent-livingbrain-test/chi-web/");
        assert_eq!(facts.folder.as_deref(), Some("chi-web"));
        assert_eq!(facts.remote_owner, None);
    }
}
