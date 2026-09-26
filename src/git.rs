//! Git integration.
//!
//! This is the difference between "your tests got slower" and "your tests got
//! slower starting at 4f2a1c9, which added a fixture that makes a network
//! call". Every run records the sha it ran against, and regression onset is
//! detected on a run, so a commit range is always available to name.
//!
//! Every call sets `GIT_OPTIONAL_LOCKS=0`. A profiler runs inside hooks and CI
//! jobs where taking the index lock would fight with the build for it, and we
//! only ever read.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct GitFacts {
    pub sha: String,
    pub branch: String,
    pub dirty: bool,
    /// True when we are inside a work tree at all.
    pub in_repo: bool,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub date_ms: i64,
    pub subject: String,
}

fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The top level of the work tree containing `dir`, if any.
pub fn repo_root() -> Option<PathBuf> {
    let dir = std::env::current_dir().ok()?;
    let out = git_in(&dir, &["rev-parse", "--show-toplevel"])?;
    if out.is_empty() {
        None
    } else {
        Some(PathBuf::from(out))
    }
}

fn current_dir_or_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Read the current sha, branch and dirty flag.
///
/// Every field degrades independently: a shallow clone still has a sha, a
/// detached HEAD still has a sha, and a directory that is not a repo at all
/// still returns a usable (empty) `GitFacts` rather than an error.
pub fn facts() -> GitFacts {
    let dir = current_dir_or_root();
    let root = git_in(&dir, &["rev-parse", "--show-toplevel"]);
    if root.as_deref().map(|r| r.is_empty()).unwrap_or(true) {
        return GitFacts::default();
    }
    let sha = git_in(&dir, &["rev-parse", "HEAD"]).unwrap_or_default();
    // `--abbrev-ref` prints the literal string "HEAD" when detached, which
    // would be misleading in a report column.
    let raw_branch = git_in(&dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let branch = if raw_branch == "HEAD" || raw_branch.is_empty() {
        String::new()
    } else {
        raw_branch
    };
    let dirty = git_in(&dir, &["status", "--porcelain", "--untracked-files=no"])
        .map(|out| !out.is_empty())
        .unwrap_or(false);
    GitFacts {
        sha,
        branch,
        dirty,
        in_repo: true,
    }
}

/// The commits between two shas, oldest first.
///
/// Returns an empty list rather than an error when the range cannot be resolved,
/// which happens legitimately: shallow clones, rebases that dropped the old sha,
/// force pushes, and profiles recorded outside a repo. Every caller must treat
/// "no commits" as "cannot attribute", never as "nothing changed".
pub fn commit_range(from: &str, to: &str) -> Vec<Commit> {
    if from.is_empty() || to.is_empty() || from == to {
        return Vec::new();
    }
    let dir = current_dir_or_root();
    // `%x00` separators keep a subject containing a space or a tab from
    // shifting every later field.
    let spec = "--pretty=format:%H%x00%h%x00%an%x00%at%x00%s%x00";
    let range = format!("{from}..{to}");
    let Some(out) = git_in(&dir, &["log", spec, "--no-decorate", &range]) else {
        return Vec::new();
    };
    if out.trim().is_empty() {
        return Vec::new();
    }
    // Newest first from git; flip so the reader sees cause before effect.
    let mut commits: Vec<Commit> = out
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\0').collect();
            if fields.len() < 5 {
                return None;
            }
            Some(Commit {
                sha: fields[0].to_string(),
                short: fields[1].to_string(),
                author: fields[2].to_string(),
                date_ms: fields[3].parse().unwrap_or(0) * 1000,
                subject: fields[4].to_string(),
            })
        })
        .collect();
    commits.reverse();
    commits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Create a throwaway repo with a couple of real commits, so the git
    /// plumbing is exercised against actual objects rather than mocks.
    fn temp_repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env("GIT_AUTHOR_NAME", "Testy McTestface")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Testy McTestface")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00+0000")
                .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00+0000")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !run(&["init", "--initial-branch=main"]) {
            return None;
        }
        if !run(&["config", "user.email", "test@example.com"]) {
            return None;
        }
        if !run(&["config", "user.name", "Testy McTestface"]) {
            return None;
        }
        // Three commits, so `HEAD~2..HEAD` is a real two-commit range rather
        // than an invalid revision that git rejects.
        let subjects = [
            ("one.txt", "first commit"),
            ("two.txt", "second commit"),
            ("three.txt", "third commit"),
        ];
        for (name, subject) in subjects {
            std::fs::write(dir.path().join(name), subject).ok()?;
            if !run(&["add", name]) || !run(&["commit", "-m", subject]) {
                return None;
            }
        }
        Some(dir)
    }

    /// The working directory is process-global, so these tests cannot run
    /// concurrently and must always put it back — including when the body
    /// panics. Getting this wrong is not hypothetical: a test that leaves the
    /// cwd inside a TempDir which then drops deletes the cwd, and every later
    /// `set_current_dir` fails with ENOENT, cascading into unrelated failures.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores the previous working directory on drop, so a panicking body
    /// cannot leak a deleted directory as the process cwd.
    struct CwdGuard(Option<std::path::PathBuf>);

    impl CwdGuard {
        fn enter(path: &std::path::Path) -> Self {
            let original = std::env::current_dir().ok();
            std::env::set_current_dir(path).expect("enter directory under test");
            Self(original)
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            if let Some(original) = self.0.take() {
                let _ = std::env::set_current_dir(original);
            }
        }
    }

    fn in_dir<T>(dir: &tempfile::TempDir, body: impl FnOnce() -> T) -> T {
        let _lock = CWD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = CwdGuard::enter(dir.path());
        body()
    }

    #[test]
    fn reads_sha_branch_and_clean_state_from_a_real_repo() {
        let Some(dir) = temp_repo() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        in_dir(&dir, || {
            let facts = facts();
            assert!(facts.in_repo);
            assert_eq!(facts.sha.len(), 40, "full sha, got {}", facts.sha);
            assert_eq!(facts.branch, "main");
            assert!(!facts.dirty, "clean tree should not be dirty");
        });
    }

    #[test]
    fn detects_a_dirty_work_tree() {
        let Some(dir) = temp_repo() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        std::fs::write(dir.path().join("one.txt"), "modified").unwrap();
        in_dir(&dir, || {
            assert!(facts().dirty, "modified tracked file must mark dirty");
        });
    }

    #[test]
    fn untracked_files_alone_do_not_count_as_dirty() {
        // A profile directory or an editor swap file should not make every run
        // look like it ran against uncommitted work.
        let Some(dir) = temp_repo() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        std::fs::write(dir.path().join("scratch.txt"), "untracked").unwrap();
        in_dir(&dir, || {
            assert!(!facts().dirty, "untracked files must not count as dirty");
        });
    }

    #[test]
    fn commit_range_returns_oldest_first_with_real_metadata() {
        let Some(dir) = temp_repo() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        in_dir(&dir, || {
            let commits = commit_range("HEAD~1", "HEAD");
            assert_eq!(commits.len(), 1, "one commit between HEAD~1 and HEAD");
            assert_eq!(commits[0].subject, "third commit");
            assert_eq!(commits[0].short.len(), 7);
            assert_eq!(commits[0].author, "Testy McTestface");
            assert!(commits[0].date_ms > 0);
        });
    }

    #[test]
    fn commit_range_over_the_whole_history_is_ordered_cause_before_effect() {
        let Some(dir) = temp_repo() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        in_dir(&dir, || {
            let commits = commit_range("HEAD~2", "HEAD");
            assert_eq!(commits.len(), 2);
            assert_eq!(
                commits[0].subject, "second commit",
                "oldest commit must come first"
            );
            assert_eq!(commits[1].subject, "third commit");
        });
    }

    #[test]
    fn an_unresolvable_range_returns_empty_not_an_error() {
        let Some(dir) = temp_repo() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        in_dir(&dir, || {
            // These are the real-world cases: force pushes, rebases, shallow
            // clones, and runs recorded before the repo had any commits.
            assert!(commit_range("", "HEAD").is_empty(), "empty from-sha");
            assert!(commit_range("HEAD", "").is_empty(), "empty to-sha");
            assert!(commit_range("HEAD", "HEAD").is_empty(), "same sha");
            assert!(
                commit_range("0000000000000000000000000000000000000000", "HEAD").is_empty(),
                "unknown sha must not panic or invent commits"
            );
        });
    }

    #[test]
    fn outside_a_repo_everything_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        // A temp dir can sit inside a work tree on some machines, so neutralise
        // discovery with an explicit non-repo.
        let _lock = CWD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = CwdGuard::enter(dir.path());
        let previous = std::env::var("GIT_CEILING_DIRECTORIES").ok();
        std::env::set_var("GIT_CEILING_DIRECTORIES", dir.path());
        let facts = facts();
        assert!(!facts.in_repo);
        assert!(facts.sha.is_empty());
        assert!(facts.branch.is_empty());
        match previous {
            Some(previous) => std::env::set_var("GIT_CEILING_DIRECTORIES", previous),
            None => std::env::remove_var("GIT_CEILING_DIRECTORIES"),
        }
    }
}
