//! Which workspace a session's upload lands in.
//!
//! One Mac can sync sessions from several repositories into several
//! workspaces. A repository's sessions go, in order of precedence, to:
//!
//! 1. the workspace of the nearest allowed root that names one (`logs
//!    workspace`; `default` counts, and stops the routes);
//! 2. the first matching `routes` entry, judged on the repository's own git
//!    top level, its folder name and its `origin` remote;
//! 3. the default workspace, but only when every fact the routes needed was
//!    readable. A route that could not be decided (no checkout, a git
//!    failure, an unrecognised remote host) holds the session instead:
//!    sending it to the default would be a leak in the other direction.
//!
//! Everything here is local. The only outside process is `git`, read-only,
//! run with the caller's repository overrides removed and a time limit.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The workspace a session goes to when nothing else says: the token
/// `livingbrain login` made. Also the spelling `logs workspace <path> default`
/// stores to mean "the default, even when a route would pick another".
pub const DEFAULT: &str = "default";

/// The word `logs workspace <path> auto` takes to clear a root's explicit
/// workspace, so its routes decide again. Never a workspace id.
pub const AUTO: &str = "auto";

/// The longest workspace id this CLI will store or send.
const MAX_TARGET_BYTES: usize = 128;

/// The host a `remote_owner` route reads when it names none.
const GITHUB: &str = "github.com";

/// How long one `git` call may run before its fact counts as unknown.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variables that would point `git -C <dir>` at another
/// repository or change what it reads. Removed from every `git` call, so a
/// repository override inherited by the CLI cannot redirect the probe.
const SCRUBBED_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CEILING_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// One automatic rule: sessions whose repository matches go to `workspace`.
/// Exactly one of `remote_owner` (the owner of `origin`, on `host`) or
/// `folder_glob` (a pattern over the repository's folder name) is set. An
/// unknown key is refused rather than ignored: a misspelled matcher would
/// otherwise send that repository's sessions to the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_glob: Option<String>,
    /// The git host a `remote_owner` route reads (default `github.com`). A
    /// remote on any other host is not decided by the route: it holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
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
        if let Some(host) = &self.host {
            if self.remote_owner.is_none() {
                return Err("`host` only applies to a remote_owner route".to_owned());
            }
            if host.is_empty()
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
            {
                return Err(format!("`{host}` is not a git host name"));
            }
        }
        check_target(&self.workspace)
    }

    /// Whether the route matches the repository. `Err` when the fact the
    /// route needs could not be read: the route cannot decide, so neither can
    /// any later one.
    fn matches(&self, repo: &Repo) -> Result<bool, String> {
        if let Some(owner) = &self.remote_owner {
            let expected = self.host.as_deref().unwrap_or(GITHUB);
            let remote = repo.remote.as_ref().map_err(Clone::clone)?;
            if !remote.host.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "the remote is on {}, not {expected}; a route on that host decides it",
                    remote.host
                ));
            }
            return Ok(remote.owner.eq_ignore_ascii_case(owner));
        }
        Ok(self
            .folder_glob
            .as_deref()
            .is_some_and(|glob| glob_match(glob, &repo.folder)))
    }

    /// A short description for `logs status` and the report.
    pub fn describe(&self) -> String {
        match (&self.remote_owner, &self.folder_glob) {
            (Some(owner), _) => format!(
                "remote_owner {owner} on {}",
                self.host.as_deref().unwrap_or(GITHUB)
            ),
            (_, Some(glob)) => format!("folder_glob {glob}"),
            _ => "no matcher".to_owned(),
        }
    }
}

/// A repository as git describes it: its canonical top level, its folder
/// name, and its `origin` remote (or the only remote). `remote` is `Err` with
/// the reason when it could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub top: String,
    pub folder: String,
    pub remote: Result<Remote, String>,
}

/// A parsed remote URL: its host and the owner segment of its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    pub owner: String,
}

/// The repositories a run has probed, one probe per top level. A session's
/// working directories all go through here, so the probe of one repository
/// decides only for that repository, whatever order they arrive in.
#[derive(Default)]
pub struct Repos {
    by_top: BTreeMap<String, Result<Repo, String>>,
}

impl Repos {
    /// The repository `cwd` is in, probed once per top level.
    pub fn of(&mut self, cwd: &str) -> Result<&Repo, String> {
        let top = git(cwd, &["rev-parse", "--show-toplevel"])
            .map(|out| out.trim().to_owned())
            .filter(|top| !top.is_empty())
            .ok_or_else(|| format!("{cwd} is not a readable git checkout"))?;
        self.by_top
            .entry(top.clone())
            .or_insert_with(|| probe_top(&top))
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// Probe a repository by its top level.
fn probe_top(top: &str) -> Result<Repo, String> {
    let folder = top
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(top)
        .to_owned();
    Ok(Repo {
        top: top.to_owned(),
        folder,
        remote: remote_of(top),
    })
}

/// The remote a route reads: `origin` when there is one, else the only
/// remote; several remotes with no `origin` is unknown.
fn remote_of(top: &str) -> Result<Remote, String> {
    let names = git(top, &["remote"]).unwrap_or_default();
    let names: Vec<&str> = names
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    let name = if names.contains(&"origin") {
        "origin"
    } else {
        match names.as_slice() {
            [only] => only,
            [] => return Err("the repository has no remote".to_owned()),
            _ => return Err("several remotes and no origin".to_owned()),
        }
    };
    let url = git(top, &["remote", "get-url", name])
        .ok_or_else(|| format!("could not read remote {name}"))?;
    parse_remote(url.trim()).ok_or_else(|| format!("remote {name} is not a recognised URL"))
}

/// A `git` command for `cwd`: the repository overrides removed, no system
/// configuration, and no filesystem monitor (which runs a configured program).
fn git_command(cwd: &str, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for key in SCRUBBED_GIT_ENV {
        command.env_remove(key);
    }
    command
}

/// Run `git -C <cwd> <args>` and return its stdout. `None` on any failure,
/// including a refusal by git's own safety checks and a call that outlives
/// [`GIT_TIMEOUT`].
fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let mut child = git_command(cwd, args).spawn().ok()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Parse a remote URL into its host and owner: `https://`, `ssh://` and the
/// scp form `git@host:owner/repo.git`, with any userinfo, port or `.git`
/// suffix. The host is lower-cased. `None` for a URL with no owner segment.
pub fn parse_remote(url: &str) -> Option<Remote> {
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
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let owner = path.trim_start_matches('/').split('/').next()?;
    (!owner.is_empty()).then(|| Remote {
        host: host.to_ascii_lowercase(),
        owner: owner.to_owned(),
    })
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

/// Whether `id` may name a workspace, or be the word `default`: non-empty, at
/// most [`MAX_TARGET_BYTES`], only ASCII letters, digits, `-`, `_`, `.`, and
/// not the reserved word `auto`.
pub fn check_target(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id != AUTO
        && id.len() <= MAX_TARGET_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`{id}` is not a workspace id (letters, digits, `-`, `_`, `.`; at most {MAX_TARGET_BYTES} characters; `auto` is reserved)"
        ))
    }
}

/// The first route that matches, as `(workspace, description)`. `Err` when a
/// route before any match needed a fact that could not be read.
pub fn first_match<'a>(
    routes: &'a [Route],
    repo: &Repo,
) -> Result<Option<(&'a str, String)>, String> {
    for route in routes {
        if route.matches(repo)? {
            return Ok(Some((route.workspace.as_str(), route.describe())));
        }
    }
    Ok(None)
}

/// Where one session goes, after every working directory it recorded has
/// been resolved. A session is one upload, so all of its directories must
/// agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The default workspace (`login`'s token).
    Default,
    /// A named workspace; its token is read for this target alone.
    Workspace(String),
    /// A route needed a fact that could not be read. Held, sent nowhere; the
    /// reason is shown to the reader.
    Unroutable(String),
    /// Its directories resolve to different workspaces. Sent nowhere.
    Mixed,
}

impl Destination {
    /// The word the report and the per-workspace groups use: `default`, the
    /// workspace id, `unroutable` or `mixed`.
    pub fn key(&self) -> &str {
        match self {
            Self::Default => DEFAULT,
            Self::Workspace(id) => id,
            Self::Unroutable(_) => "unroutable",
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

    /// Combine the destinations of a session's directories. Any unroutable
    /// directory holds the whole session; otherwise all the same means that
    /// one, and any disagreement is [`Destination::Mixed`]. No directory means
    /// the default.
    pub fn combine(destinations: impl Iterator<Item = Self>) -> Self {
        let destinations: Vec<Self> = destinations.collect();
        if let Some(held) = destinations
            .iter()
            .find(|d| matches!(d, Self::Unroutable(_)))
        {
            return held.clone();
        }
        match destinations.first() {
            None => Self::Default,
            Some(first) if destinations.iter().all(|next| next == first) => first.clone(),
            Some(_) => Self::Mixed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Destination, Remote, Repo, Repos, Route, check_target, first_match, glob_match,
        parse_remote,
    };

    fn route(key: &str, value: &str, workspace: &str) -> Route {
        Route {
            remote_owner: (key == "remote_owner").then(|| value.to_owned()),
            folder_glob: (key == "folder_glob").then(|| value.to_owned()),
            host: None,
            workspace: workspace.to_owned(),
        }
    }

    /// A repository with a github.com remote owned by `owner`.
    fn github(folder: &str, owner: &str) -> Repo {
        Repo {
            top: format!("/code/{folder}"),
            folder: folder.to_owned(),
            remote: Ok(Remote {
                host: "github.com".into(),
                owner: owner.into(),
            }),
        }
    }

    /// Both URL forms GitHub remotes come in, with and without a `.git`
    /// suffix, userinfo, a port, and an uppercase host.
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
            let remote = parse_remote(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(remote.host, "github.com", "{url}");
            assert_eq!(remote.owner, "CHI-Ecosystem", "{url}");
        }
    }

    /// A bare host, a malformed URL or an empty owner names no remote.
    #[test]
    fn a_malformed_remote_names_nothing() {
        for url in [
            "https://github.com/",
            "git@github.com:",
            "/srv/git/local.git",
            "",
            "https:///owner/repo",
        ] {
            assert_eq!(parse_remote(url), None, "{url}");
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
    fn workspace_ids_are_plain_and_the_reserved_words_are_not_ids() {
        assert!(check_target("ws-chi_1.2").is_ok());
        assert!(check_target("default").is_ok(), "the explicit default");
        for bad in [
            "",
            "has space",
            "a/b",
            "a#b",
            "émoji",
            "auto",
            &"x".repeat(129),
        ] {
            assert!(check_target(bad).is_err(), "{bad}");
        }
    }

    /// A route names exactly one matcher and a valid workspace; `host` is
    /// only for a remote_owner route and must be a host name.
    #[test]
    fn a_route_names_exactly_one_matcher() {
        assert!(
            route("remote_owner", "CHI-Ecosystem", "ws")
                .validate()
                .is_ok()
        );
        assert!(route("folder_glob", "chi-*", "ws").validate().is_ok());
        assert!(route("folder_glob", "chi-*", "ws ws").validate().is_err());
        assert!(route("folder_glob", "", "ws").validate().is_err());
        let both = Route {
            remote_owner: Some("CHI-Ecosystem".into()),
            folder_glob: Some("chi-*".into()),
            host: None,
            workspace: "ws".into(),
        };
        assert!(both.validate().is_err());
        let neither = Route {
            remote_owner: None,
            folder_glob: None,
            host: None,
            workspace: "ws".into(),
        };
        assert!(neither.validate().is_err());
        let ghe = Route {
            remote_owner: Some("acme".into()),
            folder_glob: None,
            host: Some("git.acme.example".into()),
            workspace: "ws".into(),
        };
        assert!(ghe.validate().is_ok());
        let host_on_glob = Route {
            host: Some("git.acme.example".into()),
            ..route("folder_glob", "x", "ws")
        };
        assert!(host_on_glob.validate().is_err());
    }

    /// The first matching route decides, owners compare case-insensitively,
    /// and a route that does not match falls through to the next.
    #[test]
    fn the_first_matching_route_wins() {
        let routes = vec![
            route("remote_owner", "CHI-Ecosystem", "ws-chi"),
            route("folder_glob", "chi-*", "ws-folder"),
            route("folder_glob", "*", "ws-catch-all"),
        ];
        let owned = github("chi-web", "chi-ecosystem");
        assert_eq!(
            first_match(&routes, &owned)
                .expect("decided")
                .map(|(w, _)| w),
            Some("ws-chi")
        );
        let folder_only = github("chi-web", "someone");
        assert_eq!(
            first_match(&routes, &folder_only)
                .expect("decided")
                .map(|(w, _)| w),
            Some("ws-folder")
        );
        let stranger = github("notes", "someone");
        let (target, why) = first_match(&routes, &stranger)
            .expect("decided")
            .expect("matched");
        assert_eq!(target, "ws-catch-all");
        assert_eq!(why, "folder_glob *");
    }

    /// Hold semantics: an owner route on a repository whose remote cannot be
    /// read is undecided, not a non-match. The same holds for a remote on
    /// another host, which is how a GHE or ssh-alias remote shows up.
    #[test]
    fn an_unreadable_or_foreign_remote_is_undecided_never_a_non_match() {
        let routes = vec![route("remote_owner", "CHI-Ecosystem", "ws-chi")];
        let no_remote = Repo {
            top: "/code/x".into(),
            folder: "x".into(),
            remote: Err("the repository has no remote".into()),
        };
        assert!(first_match(&routes, &no_remote).is_err());
        let gitlab = Repo {
            top: "/code/x".into(),
            folder: "x".into(),
            remote: Ok(Remote {
                host: "gitlab.com".into(),
                owner: "CHI-Ecosystem".into(),
            }),
        };
        assert!(first_match(&routes, &gitlab).is_err());
        let ghe = Route {
            host: Some("git.acme.example".into()),
            ..route("remote_owner", "acme", "ws-acme")
        };
        let acme = Repo {
            top: "/code/x".into(),
            folder: "x".into(),
            remote: Ok(Remote {
                host: "git.acme.example".into(),
                owner: "acme".into(),
            }),
        };
        assert_eq!(
            first_match(std::slice::from_ref(&ghe), &acme)
                .expect("decided")
                .map(|(w, _)| w),
            Some("ws-acme")
        );
        // A folder route still decides a repository whose remote is unknown.
        let folder = [route("folder_glob", "x", "ws-folder")];
        assert_eq!(
            first_match(&folder, &no_remote)
                .expect("decided")
                .map(|(w, _)| w),
            Some("ws-folder")
        );
    }

    /// Every directory of a session must agree; an unroutable one holds the
    /// whole session, and any disagreement is mixed.
    #[test]
    fn a_session_resolves_to_one_destination_or_is_held_or_mixed() {
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
            Destination::combine([ws("a"), Destination::Unroutable("why".into())].into_iter()),
            Destination::Unroutable("why".into())
        );
        assert_eq!(
            Destination::combine(std::iter::empty()),
            Destination::Default
        );
        assert_eq!(Destination::of_target("default"), Destination::Default);
        assert_eq!(Destination::of_target("ws-chi").key(), "ws-chi");
        assert_eq!(Destination::Unroutable("x".into()).key(), "unroutable");
    }

    /// A git overrides inherited from the CLI's environment are removed from
    /// every probe, and system configuration is off.
    #[test]
    fn git_ignores_inherited_repository_overrides() {
        let command = super::git_command("/code/x", &["rev-parse", "--show-toplevel"]);
        let envs: Vec<_> = command.get_envs().collect();
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(
                envs.iter()
                    .any(|(k, v)| k.to_string_lossy() == key && v.is_none()),
                "{key} must be removed"
            );
        }
        assert!(
            envs.iter()
                .any(|(k, v)| k.to_string_lossy() == "GIT_CONFIG_NOSYSTEM"
                    && v.is_some_and(|v| v.to_string_lossy() == "1"))
        );
    }

    /// Two repositories under one parent are probed separately: each gets its
    /// own top level and remote, whatever order they are asked in.
    #[test]
    fn repositories_under_one_root_are_probed_separately() {
        let base = std::env::temp_dir().join(format!("lb-route-{}", std::process::id()));
        let chi = base.join("chi-web");
        let other = base.join("notes");
        for (dir, url) in [
            (&chi, "https://github.com/CHI-Ecosystem/chi-web.git"),
            (&other, "https://github.com/someone/notes.git"),
        ] {
            std::fs::create_dir_all(dir).expect("dir");
            let init = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["init", "-q"])
                .status();
            if init.map(|s| !s.success()).unwrap_or(true) {
                let _ = std::fs::remove_dir_all(&base);
                return; // git unavailable: nothing to probe
            }
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["remote", "add", "origin", url])
                .status();
        }
        let routes = [route("remote_owner", "CHI-Ecosystem", "ws-chi")];
        let mut repos = Repos::default();
        let other_cwd = other.to_string_lossy().into_owned();
        let chi_cwd = chi.to_string_lossy().into_owned();
        for order in [[&other_cwd, &chi_cwd], [&chi_cwd, &other_cwd]] {
            for cwd in order {
                let repo = repos.of(cwd).expect("a repository").clone();
                let target = first_match(&routes, &repo)
                    .expect("decided")
                    .map(|(w, _)| w.to_owned());
                if cwd == &chi_cwd {
                    assert_eq!(target.as_deref(), Some("ws-chi"));
                } else {
                    assert_eq!(target, None, "someone/notes is not CHI");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
