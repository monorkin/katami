//! What a project is called, so its memories find it on every machine.
//!
//! A project's memories are filed under one entity string, and a session
//! finds them by working out that string from its working directory. A path
//! can't be that string once memory is shared: the same repository lives at
//! `/home/stanko/Work/app` on one machine and `/home/agent/code/app` on the
//! next. The `origin` remote is the same everywhere, so a checkout is named
//! after it — normalized, because `git@github.com:acme/app.git` and
//! `https://github.com/acme/app` are one place written two ways. A fork has
//! its own origin and so its own memories, which is right more often than
//! not. Anything without an origin keeps its path, written from `~` when it's
//! under the home directory: two machines with the same layout then agree on
//! it despite different usernames, and two with different layouts are no
//! worse off.
//!
//! Every path a project is reached through — the working directory, its
//! symlink-resolved form, the main checkout behind a worktree — is an alias
//! for that name, and the store re-homes memories from aliases, which is also
//! how a store full of path-named projects migrates itself.
//!
//! A remote can change under a checkout: the repository was renamed, or moved
//! hosts. The root commit is what says it's still the same repository, as
//! opposed to a scratch directory that now holds something else entirely, so
//! only a matching root commit lets the old name become an alias for the new.
//! A shallow clone has no trustworthy root, and reports none.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Project {
    pub entity: String,
    pub aliases: Vec<String>,
    pub root_commit: Option<String>,
}

pub fn at(cwd: &Path) -> Project {
    let resolved = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let checkout = git_main_root(&resolved);
    let home = checkout.clone().unwrap_or_else(|| resolved.clone());

    let entity = checkout
        .as_deref()
        .and_then(origin)
        .map(|it| format!("project:{it}"))
        .unwrap_or_else(|| path_entity(&home));

    let mut aliases = Vec::new();
    for path in [cwd, &resolved, &home] {
        for alias in [path_entity(path), absolute_entity(path)] {
            if alias != entity && !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
    }

    Project {
        entity,
        aliases,
        root_commit: checkout.as_deref().and_then(root_commit),
    }
}

pub fn path_entity(path: &Path) -> String {
    name_path(path, dirs::home_dir().as_deref())
}

/// A path under the home directory is written from `~`, because the user's
/// name is the part of a path most likely to differ between their machines.
pub fn name_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|it| path.strip_prefix(it).ok()) {
        Some(below_home) if below_home.as_os_str().is_empty() => "project:~".to_string(),
        Some(below_home) => format!("project:~/{}", below_home.display()),
        None => absolute_entity(path),
    }
}

/// Where a path-named project lives on this machine; `None` for a project
/// named after its remote, which could be checked out anywhere or nowhere.
pub fn local_path(entity: &str) -> Option<PathBuf> {
    let name = entity.strip_prefix("project:")?;
    if name == "~" {
        dirs::home_dir()
    } else if let Some(below_home) = name.strip_prefix("~/") {
        dirs::home_dir().map(|it| it.join(below_home))
    } else if name.starts_with('/') {
        Some(PathBuf::from(name))
    } else {
        None
    }
}

fn absolute_entity(path: &Path) -> String {
    format!("project:{}", path.display())
}

/// `host/owner/repo` for any spelling of a network remote, and `None` for a
/// remote that's a path on this machine, which names nothing anywhere else.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let location = if let Some((_, rest)) = url.split_once("://") {
        if url.starts_with("file://") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |it| it.1);
        let host = host.split_once(':').map_or(host, |it| it.0);
        format!("{host}/{path}")
    } else {
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') {
            return None;
        }
        let host = authority.rsplit_once('@').map_or(authority, |it| it.1);
        format!("{host}/{}", path.trim_start_matches('/'))
    };

    let location = location.trim_end_matches('/');
    let location = location.strip_suffix(".git").unwrap_or(location).to_lowercase();
    if location.split('/').filter(|it| !it.is_empty()).count() >= 2 {
        Some(location)
    } else {
        None
    }
}

fn origin(checkout: &Path) -> Option<String> {
    normalize_remote(&git(checkout, &["remote", "get-url", "origin"])?)
}

fn root_commit(checkout: &Path) -> Option<String> {
    if git(checkout, &["rev-parse", "--is-shallow-repository"])? == "false" {
        git(checkout, &["rev-list", "--max-parents=0", "HEAD"])?
            .lines()
            .min()
            .map(str::to_string)
    } else {
        None
    }
}

fn git_main_root(cwd: &Path) -> Option<PathBuf> {
    let common_dir = PathBuf::from(git(cwd, &["rev-parse", "--git-common-dir"])?);
    let common_dir = if common_dir.is_relative() {
        cwd.join(common_dir)
    } else {
        common_dir
    };
    common_dir.canonicalize().ok()?.parent().map(Path::to_path_buf)
}

fn git(directory: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("katami-project-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory.canonicalize().unwrap()
    }

    fn run_git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
            .args(arguments)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "git {arguments:?} failed");
    }

    #[test]
    fn every_spelling_of_a_remote_is_one_name() {
        let spellings = [
            "git@github.com:Acme/App.git",
            "https://github.com/acme/app",
            "https://github.com/acme/app.git/",
            "https://someone:token@github.com/acme/app.git",
            "ssh://git@github.com:22/acme/app.git",
            "git://github.com/acme/app",
        ];
        for spelling in spellings {
            assert_eq!(normalize_remote(spelling).as_deref(), Some("github.com/acme/app"), "{spelling}");
        }
        assert_eq!(
            normalize_remote("git@gitlab.example.com:group/sub/app.git").as_deref(),
            Some("gitlab.example.com/group/sub/app")
        );
    }

    #[test]
    fn paths_under_home_are_named_from_the_tilde() {
        let home = Path::new("/home/someone");
        assert_eq!(name_path(Path::new("/home/someone/Work/app"), Some(home)), "project:~/Work/app");
        assert_eq!(name_path(home, Some(home)), "project:~");
        assert_eq!(name_path(Path::new("/srv/app"), Some(home)), "project:/srv/app");
        assert_eq!(name_path(Path::new("/home/someone-else/app"), Some(home)), "project:/home/someone-else/app");
        assert_eq!(name_path(Path::new("/home/someone/app"), None), "project:/home/someone/app");

        assert_eq!(local_path("project:/srv/app"), Some(PathBuf::from("/srv/app")));
        assert_eq!(local_path("project:~/Work/app"), dirs::home_dir().map(|it| it.join("Work/app")));
        assert_eq!(local_path("project:~"), dirs::home_dir());
        assert_eq!(local_path("project:example.com/acme/app"), None);
        assert_eq!(local_path("person:Jason"), None);
    }

    #[test]
    fn remotes_that_are_local_paths_name_nothing() {
        for local in ["/srv/git/app.git", "../app", "file:///srv/git/app.git", "./mirror:old", "https://github.com"] {
            assert_eq!(normalize_remote(local), None, "{local}");
        }
    }

    #[test]
    fn a_checkout_is_named_after_its_origin_wherever_it_lives() {
        let first = scratch("first");
        run_git(&first, &["init", "-q"]);
        run_git(&first, &["commit", "-q", "--allow-empty", "-m", "Start"]);
        run_git(&first, &["remote", "add", "origin", "git@example.com:acme/app.git"]);

        let project = at(&first);
        assert_eq!(project.entity, "project:example.com/acme/app");
        assert_eq!(project.aliases, vec![path_entity(&first)]);
        assert_eq!(project.root_commit.as_ref().map(String::len), Some(40));

        let subdirectory = first.join("src");
        std::fs::create_dir_all(&subdirectory).unwrap();
        let nested = at(&subdirectory);
        assert_eq!(nested.entity, project.entity);
        assert_eq!(nested.aliases, vec![path_entity(&subdirectory), path_entity(&first)]);

        let worktree = scratch("worktree");
        std::fs::remove_dir_all(&worktree).unwrap();
        run_git(&first, &["worktree", "add", "-q", worktree.to_str().unwrap()]);
        let linked = at(&worktree);
        assert_eq!(linked.entity, project.entity);
        assert_eq!(linked.root_commit, project.root_commit);
        assert_eq!(linked.aliases, vec![path_entity(&worktree), path_entity(&first)]);

        std::fs::remove_dir_all(&worktree).unwrap();
        std::fs::remove_dir_all(&first).unwrap();
    }

    #[test]
    fn without_an_origin_a_project_keeps_its_path() {
        let plain = scratch("plain");
        let project = at(&plain);
        assert_eq!(project.entity, path_entity(&plain));
        assert!(project.aliases.is_empty());
        assert_eq!(project.root_commit, None);

        run_git(&plain, &["init", "-q"]);
        run_git(&plain, &["commit", "-q", "--allow-empty", "-m", "Start"]);
        let local_only = at(&plain);
        assert_eq!(local_only.entity, path_entity(&plain));
        assert!(local_only.root_commit.is_some());

        std::fs::remove_dir_all(&plain).unwrap();
    }
}
