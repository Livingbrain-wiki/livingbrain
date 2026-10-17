//! The Colonizer registry: the colonies Colonizer runs in microVMs, and the
//! repository each one works on.
//!
//! A colony's transcripts sit at `sessions/<id>/transcripts/-workspace` under
//! the Colonizer directory, in Claude Code's format. Every record says the
//! guest path `/workspace`, so nothing in the transcript names the repository.
//! `sessions.json` does: each colony's `repo`, `Owner/name`. A colony is then
//! judged as if it ran in the virtual root `colonizer://<owner>/<name>`, which
//! the allowlist and the routes read like a checkout's path and its remote.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::route::{Remote, Repo};
use crate::{CliResult, err};

/// The scheme of a colony's virtual root.
pub const SCHEME: &str = "colonizer://";

/// The host every colony's repository is on: routes read it as GitHub.
const GITHUB: &str = "github.com";

/// One colony the registry names a repository for.
#[derive(Debug, Clone)]
pub struct Colony {
    /// The colony's id: its directory name under `sessions/`.
    pub id: String,
    /// The virtual root, `colonizer://<owner>/<name>`.
    pub url: String,
    /// Where its transcripts sit: `sessions/<id>/transcripts/-workspace`.
    pub transcripts: PathBuf,
}

/// Whether a path or a working directory is a colony's virtual root, rather
/// than a directory on this machine.
pub fn is_root(text: &str) -> bool {
    text.starts_with(SCHEME)
}

/// One GitHub name: letters, digits, `-`, `_` and `.`, and not `.` or `..`.
fn segment_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The virtual root of the repository `repo` names, `Owner/name`. `None` for
/// anything else.
pub fn url_of(repo: &str) -> Option<String> {
    let (owner, name) = repo.split_once('/')?;
    (segment_ok(owner) && segment_ok(name)).then(|| format!("{SCHEME}{owner}/{name}"))
}

/// The stored form of a `logs allow` argument naming a colony root:
/// `colonizer://<owner>` for an organisation, or `colonizer://<owner>/<name>`
/// for one repository.
pub fn root_of(raw: &str) -> Result<String, String> {
    let rest = raw
        .strip_prefix(SCHEME)
        .map(|rest| rest.trim_end_matches('/'))
        .ok_or_else(|| format!("`{raw}` is not a Colonizer root"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [owner] | [owner, _] if parts.iter().all(|part| segment_ok(part)) => {
            Ok(format!("{SCHEME}{rest}"))
        }
        _ => Err(format!(
            "`{raw}` is not a Colonizer root: use colonizer://<owner> or colonizer://<owner>/<name>"
        )),
    }
}

/// The owner and name segments of a virtual root.
fn segments(url: &str) -> Option<Vec<&str>> {
    url.strip_prefix(SCHEME)
        .map(|rest| rest.split('/').collect())
}

/// Whether `child` is `ancestor` itself or inside it: an organisation holds
/// its repositories. Names compare case-insensitively, as GitHub's do.
pub fn inside(child: &str, ancestor: &str) -> bool {
    let (Some(child), Some(ancestor)) = (segments(child), segments(ancestor)) else {
        return false;
    };
    child.len() >= ancestor.len()
        && child
            .iter()
            .zip(ancestor.iter())
            .all(|(c, a)| c.eq_ignore_ascii_case(a))
}

/// Whether a virtual root names an organisation rather than a repository.
pub fn is_org(url: &str) -> bool {
    segments(url).is_some_and(|parts| parts.len() == 1)
}

/// The repository a virtual root names, for the routes: its owner on GitHub
/// and its name. `None` for an organisation, which names no repository.
pub fn repo_of(url: &str) -> Option<Repo> {
    let parts = segments(url)?;
    let [owner, name] = parts.as_slice() else {
        return None;
    };
    Some(Repo {
        top: url.to_owned(),
        folder: (*name).to_owned(),
        remote: Ok(Remote {
            host: GITHUB.to_owned(),
            owner: (*owner).to_owned(),
        }),
    })
}

/// The colonies `dir/sessions.json` names a repository for and whose
/// directory is here, in id order. A colony with no repository, or one that
/// is not `Owner/name`, is left out: nothing can route it. A missing registry
/// is no colonies; one that is not JSON is an error, not an empty list.
pub fn colonies(dir: &Path) -> CliResult<Vec<Colony>> {
    let path = dir.join("sessions.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(err(format!("could not read {}: {error}", path.display()))),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| err(format!("{}: not valid JSON ({error})", path.display())))?;
    let repos = repos_by_id(&value);
    let Ok(entries) = std::fs::read_dir(dir.join("sessions")) else {
        return Ok(Vec::new());
    };
    let mut colonies: Vec<Colony> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let id = entry.file_name().into_string().ok()?;
            let url = url_of(repos.get(&id)?)?;
            Some(Colony {
                transcripts: entry.path().join("transcripts").join("-workspace"),
                id,
                url,
            })
        })
        .collect();
    colonies.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(colonies)
}

/// The repository of each registry entry, by colony id. The registry is a JSON
/// array of colonies, or an object whose `sessions` is an array or a map keyed
/// by id; a bare object is read as that map. Unknown fields are ignored.
fn repos_by_id(value: &Value) -> BTreeMap<String, String> {
    let mut repos = BTreeMap::new();
    match value.get("sessions").unwrap_or(value) {
        Value::Array(items) => {
            for item in items {
                note(&mut repos, None, item);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                note(&mut repos, Some(key.as_str()), item);
            }
        }
        _ => {}
    }
    repos
}

/// Record one registry entry's repository under its id, when it has both.
fn note(repos: &mut BTreeMap<String, String>, key: Option<&str>, meta: &Value) {
    let id = meta.get("id").and_then(Value::as_str).or(key);
    let repo = meta
        .get("repo")
        .and_then(Value::as_str)
        .filter(|repo| url_of(repo).is_some());
    if let (Some(id), Some(repo)) = (id, repo) {
        repos.insert(id.to_owned(), repo.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{inside, is_org, repo_of, root_of, url_of};
    use serde_json::Value;

    #[test]
    fn a_repository_is_a_virtual_root_with_its_owner_and_name() {
        assert_eq!(
            url_of("CHI-Ecosystem/chi-backend").as_deref(),
            Some("colonizer://CHI-Ecosystem/chi-backend")
        );
        for bad in ["", "owner", "owner/", "/name", "a b/c", "../x", "a/b/c"] {
            assert_eq!(url_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_root_argument_is_an_org_or_a_repository() {
        assert_eq!(
            root_of("colonizer://CHI-Ecosystem/").as_deref(),
            Ok("colonizer://CHI-Ecosystem")
        );
        assert_eq!(
            root_of("colonizer://CHI-Ecosystem/chi-web").as_deref(),
            Ok("colonizer://CHI-Ecosystem/chi-web")
        );
        for bad in [
            "colonizer://",
            "colonizer://a/b/c",
            "colonizer://a b",
            "/home/x",
        ] {
            assert!(root_of(bad).is_err(), "{bad}");
        }
    }

    /// An organisation holds its repositories, whatever the case of either
    /// name; a sibling organisation whose name starts the same does not.
    #[test]
    fn an_org_holds_its_repositories_case_insensitively() {
        assert!(inside(
            "colonizer://chi-ecosystem/chi-web",
            "colonizer://CHI-Ecosystem"
        ));
        assert!(inside(
            "colonizer://CHI-Ecosystem/chi-web",
            "colonizer://CHI-Ecosystem/CHI-WEB"
        ));
        assert!(!inside(
            "colonizer://CHI-Ecosystem-2/chi-web",
            "colonizer://CHI-Ecosystem"
        ));
        assert!(!inside(
            "colonizer://CHI-Ecosystem",
            "colonizer://CHI-Ecosystem/chi-web"
        ));
        assert!(!inside("/home/alice/code", "colonizer://CHI-Ecosystem"));
        assert!(is_org("colonizer://CHI-Ecosystem"));
        assert!(!is_org("colonizer://CHI-Ecosystem/chi-web"));
    }

    #[test]
    fn a_repository_root_is_judged_as_a_github_remote() {
        let repo = repo_of("colonizer://CHI-Ecosystem/chi-web").expect("a repository");
        assert_eq!(repo.folder, "chi-web");
        assert_eq!(repo.top, "colonizer://CHI-Ecosystem/chi-web");
        let remote = repo.remote.expect("a remote");
        assert_eq!(remote.host, "github.com");
        assert_eq!(remote.owner, "CHI-Ecosystem");
        assert!(repo_of("colonizer://CHI-Ecosystem").is_none());
    }

    /// The registry's shapes: an array, an object with a `sessions` array, one
    /// with a `sessions` map, and a bare map. Entries without a usable repo
    /// are skipped; unknown fields are ignored.
    #[test]
    fn every_registry_shape_names_the_same_repositories() {
        let shapes = [
            r#"[{"id":"c1","repo":"CHI-Ecosystem/chi-backend","extra":1},{"id":"c2"},{"id":"c3","repo":"not a repo"}]"#,
            r#"{"version":2,"sessions":[{"id":"c1","repo":"CHI-Ecosystem/chi-backend"},{"id":"c2"}]}"#,
            r#"{"sessions":{"c1":{"repo":"CHI-Ecosystem/chi-backend"},"c2":{"name":"x"}}}"#,
            r#"{"c1":{"id":"c1","repo":"CHI-Ecosystem/chi-backend"},"c2":{"repo":""}}"#,
        ];
        for shape in shapes {
            let value: Value = serde_json::from_str(shape).expect("json");
            let repos = super::repos_by_id(&value);
            assert_eq!(
                repos.get("c1").map(String::as_str),
                Some("CHI-Ecosystem/chi-backend"),
                "{shape}"
            );
            assert_eq!(repos.len(), 1, "{shape}");
        }
    }

    /// A colony directory is read only when the registry names its repository;
    /// a registry that is not JSON is an error, a missing one is no colonies.
    #[test]
    fn colonies_come_from_the_registry_and_the_directory() {
        let dir = std::env::temp_dir().join(format!("lb-colonizer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(super::colonies(&dir).expect("missing is empty").is_empty());
        for id in ["c-chi", "c-fz", "c-unnamed"] {
            std::fs::create_dir_all(dir.join("sessions").join(id)).expect("colony dir");
        }
        std::fs::write(
            dir.join("sessions.json"),
            r#"[{"id":"c-chi","repo":"CHI-Ecosystem/chi-web"},{"id":"c-fz","repo":"Colonizer-dev/tool"},{"id":"c-unnamed"}]"#,
        )
        .expect("registry");
        let colonies = super::colonies(&dir).expect("colonies");
        let ids: Vec<(&str, &str)> = colonies
            .iter()
            .map(|c| (c.id.as_str(), c.url.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("c-chi", "colonizer://CHI-Ecosystem/chi-web"),
                ("c-fz", "colonizer://Colonizer-dev/tool"),
            ]
        );
        assert!(colonies[0].transcripts.ends_with("transcripts/-workspace"));
        std::fs::write(dir.join("sessions.json"), "not json").expect("registry");
        assert!(super::colonies(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
