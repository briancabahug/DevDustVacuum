//! Wrapper around the `git` command.

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::execute;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("could not run git: {0}")]
    Spawn(#[from] io::Error),
    #[error("git {command} timed out")]
    Timeout { command: &'static str },
    #[error("git {command} failed ({status})")]
    Failed {
        command: &'static str,
        status: ExitStatus,
    },
    #[error("git {command} printed something unexpected")]
    Unexpected { command: &'static str },
}

pub struct Git {
    timeout: Duration,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
        }
    }
}

impl Git {
    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .arg("--no-optional-locks")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Whether `path`, inside the repo at `scope`, is git-ignored.
    pub fn is_ignored(&self, scope: &Path, path: &Path) -> Result<bool, GitError> {
        const COMMAND: &str = "check-ignore";
        // check-ignore rejects --literal-pathspecs, and without it a path that
        // starts with `:` is read as pathspec magic. A leading `./` prevents that.
        let mut child = self
            .command(scope)
            .args([COMMAND, "-q", "--"])
            .arg(Path::new(".").join(relative(scope, path)))
            .spawn()?;
        let status = self.wait(&mut child, Instant::now() + self.timeout, COMMAND)?;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(GitError::Failed {
                command: COMMAND,
                status,
            }),
        }
    }

    /// Whether the repo at `scope` tracks any file inside `dir`.
    pub fn has_tracked_files(&self, scope: &Path, dir: &Path) -> Result<bool, GitError> {
        const COMMAND: &str = "ls-files";
        let mut command = self.command(scope);
        command
            .args(["--literal-pathspecs", COMMAND, "-z", "--"])
            .arg(relative(scope, dir));
        // One entry is the answer; a vendored tree can list thousands of files.
        self.any_entry(command, COMMAND, |_| true)
    }

    /// The per-worktree git dir (holds `HEAD` and `logs/HEAD`) and the shared
    /// repo dir of the repo that `dir` is the root of.
    pub fn git_dirs(&self, dir: &Path) -> Result<(PathBuf, PathBuf), GitError> {
        const COMMAND: &str = "rev-parse";
        let mut command = self.command(dir);
        command.args([
            COMMAND,
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ]);
        let text = self.text(command, COMMAND)?;
        let mut lines = text.lines();
        match (lines.next(), lines.next(), lines.next()) {
            (Some(git_dir), Some(common_dir), None) => Ok((git_dir.into(), common_dir.into())),
            _ => Err(GitError::Unexpected { command: COMMAND }),
        }
    }

    /// Committer date of the newest commit on `HEAD`, or of the newest one
    /// that touches `path`. `None` when there is no such commit.
    pub fn last_commit(
        &self,
        worktree: &Path,
        path: Option<&Path>,
    ) -> Result<Option<SystemTime>, GitError> {
        const COMMAND: &str = "log";
        let mut command = self.command(worktree);
        command.args([
            "--literal-pathspecs",
            COMMAND,
            "-1",
            "--format=%ct",
            "HEAD",
            "--",
        ]);
        if let Some(path) = path {
            command.arg(relative(worktree, path));
        }
        match self.text(command, COMMAND) {
            Ok(text) if text.trim().is_empty() => Ok(None),
            Ok(text) => match text.trim().parse() {
                Ok(seconds) => Ok(Some(UNIX_EPOCH + Duration::from_secs(seconds))),
                Err(_) => Err(GitError::Unexpected { command: COMMAND }),
            },
            // `git log HEAD` fails in a repo that has no commits yet.
            Err(GitError::Failed { .. }) if self.has_no_commits(worktree)? => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn has_no_commits(&self, worktree: &Path) -> Result<bool, GitError> {
        const COMMAND: &str = "rev-parse";
        let mut child = self
            .command(worktree)
            .args([COMMAND, "-q", "--verify", "HEAD^{commit}"])
            .spawn()?;
        let status = self.wait(&mut child, Instant::now() + self.timeout, COMMAND)?;
        match status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(GitError::Failed {
                command: COMMAND,
                status,
            }),
        }
    }

    /// Whether `path` has uncommitted changes. Untracked entries inside
    /// `dust` (paths relative to `worktree`) do not count: a dust directory
    /// that is not git-ignored always shows up as untracked. Neither does
    /// dust that an interrupted `clean` left half deleted.
    pub fn has_changes(
        &self,
        worktree: &Path,
        path: &Path,
        dust: Vec<PathBuf>,
    ) -> Result<bool, GitError> {
        const COMMAND: &str = "status";
        let mut command = self.command(worktree);
        command.args([
            "--literal-pathspecs",
            COMMAND,
            "--porcelain=v1",
            "--untracked-files=normal",
            "-z",
            "--",
        ]);
        let path = relative(worktree, path);
        if !path.as_os_str().is_empty() {
            command.arg(path);
        }
        // A rename adds a second, bare path entry, but the rename entry
        // before it already counts as a change and ends the search.
        self.any_entry(command, COMMAND, move |entry| {
            let Some(untracked) = entry.strip_prefix(b"?? ".as_slice()) else {
                return true;
            };
            let untracked = Path::new(OsStr::from_bytes(untracked));
            let being_deleted = untracked
                .components()
                .any(|part| execute::is_interrupted_deletion(part.as_os_str()));
            !being_deleted && !dust.iter().any(|dir| untracked.starts_with(dir))
        })
    }

    /// Runs git and returns everything it prints.
    fn text(&self, command: Command, name: &'static str) -> Result<String, GitError> {
        self.read(command, name, |mut stdout| {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            (text, false)
        })
    }

    /// Whether git prints a NUL-terminated entry that `wanted` accepts. Git is
    /// stopped at the first one.
    fn any_entry(
        &self,
        command: Command,
        name: &'static str,
        wanted: impl Fn(&[u8]) -> bool + Send + 'static,
    ) -> Result<bool, GitError> {
        self.read(command, name, move |stdout| {
            let found = stdout
                .split(0)
                .map_while(Result::ok)
                .any(|entry| wanted(&entry));
            (found, found)
        })
    }

    /// Runs git and hands its output to `read` on another thread, so the
    /// timeout also covers a git that stalls mid-output. `read` returns its
    /// result and whether that result is already final; if it is, git is
    /// stopped without waiting for it to finish.
    fn read<T: Send + 'static>(
        &self,
        mut command: Command,
        name: &'static str,
        read: impl FnOnce(BufReader<ChildStdout>) -> (T, bool) + Send + 'static,
    ) -> Result<T, GitError> {
        let deadline = Instant::now() + self.timeout;
        let mut child = command.stdout(Stdio::piped()).spawn()?;
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(read(stdout));
        });

        match receiver.recv_timeout(self.timeout) {
            Ok((value, true)) => {
                stop(&mut child);
                Ok(value)
            }
            Ok((value, false)) => {
                let status = self.wait(&mut child, deadline, name)?;
                if status.success() {
                    Ok(value)
                } else {
                    Err(GitError::Failed {
                        command: name,
                        status,
                    })
                }
            }
            Err(_) => {
                stop(&mut child);
                Err(GitError::Timeout { command: name })
            }
        }
    }

    fn wait(
        &self,
        child: &mut Child,
        deadline: Instant,
        command: &'static str,
    ) -> Result<ExitStatus, GitError> {
        let mut pause = Duration::from_millis(1);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                stop(child);
                return Err(GitError::Timeout { command });
            }
            thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(20));
        }
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `path` relative to `scope`, so git never has to resolve symlinks in a
/// shared prefix differently from us.
fn relative<'a>(scope: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(scope).unwrap_or(path)
}
