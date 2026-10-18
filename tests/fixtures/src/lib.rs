//! Builds throwaway project trees for tests.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

pub struct Fixture {
    root: PathBuf,
    _dir: TempDir,
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Fixture {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        // The scanner resolves its roots, and macOS temp dirs sit behind a symlink.
        let root = dir.path().canonicalize().unwrap();
        Self { root, _dir: dir }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn dir(&self, rel: &str) -> &Self {
        fs::create_dir_all(self.path(rel)).unwrap();
        self
    }

    /// Writes a file, creating its parent directories.
    pub fn file(&self, rel: &str, contents: &str) -> &Self {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        self
    }

    /// Writes a file of `len` bytes.
    pub fn sized_file(&self, rel: &str, len: usize) -> &Self {
        self.file(rel, &"x".repeat(len))
    }

    /// Runs git in `rel`, isolated from the user's and the system's config.
    pub fn git(&self, rel: &str, args: &[&str]) -> &Self {
        self.git_at(rel, 0, args)
    }

    /// Runs git in `rel` as if it were `days_ago` days ago: commits and
    /// reflog entries it writes carry that date.
    pub fn git_at(&self, rel: &str, days_ago: u64, args: &[&str]) -> &Self {
        let date = format!("@{} +0000", unix_time(days_ago));
        let output = Command::new("git")
            .current_dir(self.path(rel))
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self
    }

    /// Creates a repository in `rel`.
    pub fn git_init(&self, rel: &str) -> &Self {
        self.dir(rel).git(rel, &["init", "-q", "-b", "main"])
    }

    /// Stages and commits everything in the repository at `rel`.
    pub fn commit_all(&self, rel: &str) -> &Self {
        self.commit_all_at(rel, 0)
    }

    /// Stages and commits everything in the repository at `rel`, dated `days_ago`.
    pub fn commit_all_at(&self, rel: &str, days_ago: u64) -> &Self {
        self.git(rel, &["add", "-A"])
            .git_at(rel, days_ago, &["commit", "-q", "-m", "fixture"])
    }

    /// Sets the mtime of `rel` and everything below it to `days_ago` days ago.
    pub fn age(&self, rel: &str, days_ago: u64) -> &Self {
        let time = UNIX_EPOCH + Duration::from_secs(unix_time(days_ago));
        set_mtime(&self.path(rel), time);
        self
    }
}

fn unix_time(days_ago: u64) -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    now.as_secs() - days_ago * 24 * 60 * 60
}

fn set_mtime(path: &Path, time: SystemTime) {
    let meta = fs::symlink_metadata(path).unwrap();
    if meta.is_symlink() {
        return;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            set_mtime(&entry.unwrap().path(), time);
        }
    }
    File::open(path).unwrap().set_modified(time).unwrap();
}
