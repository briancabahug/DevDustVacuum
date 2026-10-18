//! Deletes the dust of the plan's reclaimable projects (design §8).

use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::assess::Warmth;
use crate::git::Git;
use crate::model::Dust;
use crate::plan::Project;
use crate::{Error, Scan, classify};

/// Large trees take tens of seconds each to delete; more workers than this
/// mostly fight over the disk.
const WORKERS: usize = 4;

const INTERRUPTED_PREFIX: &str = ".devdust-deleting-";

/// Whether a directory name is one that `execute` gives dust while deleting
/// it. No dust rule matches such a name, so it is never mistaken for a
/// project's own files, and a run that was killed can be finished later.
pub fn is_interrupted_deletion(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(INTERRUPTED_PREFIX))
        .is_some_and(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub struct CleanOptions {
    /// Move dust to the Trash instead of deleting it.
    pub trash: bool,
    /// The action log, one JSON line per removed directory.
    pub log_path: PathBuf,
    /// Skip dust that a running process has files open in.
    pub check_open_files: bool,
    /// The time the cold check is repeated for.
    pub now: SystemTime,
    /// The paths of the projects to clean. `None` is every reclaimable one.
    pub only: Option<HashSet<PathBuf>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Deleted,
    Trashed,
    /// A safety check refused it. Nothing was touched.
    Skipped(String),
    /// Removing it went wrong part-way.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Removal {
    pub path: PathBuf,
    pub size: u64,
    pub outcome: Outcome,
}

#[derive(Serialize)]
struct LogEntry<'a> {
    time: String,
    action: &'a str,
    path: &'a Path,
    project: &'a Path,
    rule: &'a str,
    size: u64,
    restore_hint: &'a str,
}

/// One line of the action log: a directory that was removed.
#[derive(Debug, Clone, Deserialize)]
pub struct LogRecord {
    /// RFC 3339, in UTC.
    pub time: String,
    /// `deleted` or `trashed`.
    pub action: String,
    pub path: PathBuf,
    pub project: PathBuf,
    pub rule: String,
    pub size: u64,
    pub restore_hint: String,
}

/// Reads the action log, oldest line first. A missing log is an empty one.
/// A line that is not a record, such as one cut short by a crash, is left out.
pub fn read_log(path: &Path) -> Result<Vec<LogRecord>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::LogRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

struct Context<'a> {
    scan: &'a Scan,
    options: &'a CleanOptions,
    git: Git,
    log: Mutex<File>,
    home: Option<PathBuf>,
}

/// Removes the dust of the reclaimable projects in the scan's plan, and
/// finishes deletions that an earlier run was interrupted in. Every safety
/// check runs again for each directory right before it is removed.
/// `on_done` is called as each directory is dealt with.
pub fn execute(
    scan: &Scan,
    options: &CleanOptions,
    on_done: &(dyn Fn(&Removal) + Sync),
) -> Result<Vec<Removal>, Error> {
    let log_error = |source| Error::Log {
        path: options.log_path.clone(),
        source,
    };
    // Opened before anything is deleted: no log, no deletion.
    if let Some(folder) = options.log_path.parent() {
        fs::create_dir_all(folder).map_err(log_error)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.log_path)
        .map_err(log_error)?;

    let context = Context {
        scan,
        options,
        git: Git::default(),
        log: Mutex::new(log),
        home: env::var_os("HOME").map(PathBuf::from),
    };

    let work = || {
        let interrupted = scan.plan.interrupted.par_iter().map(|path| {
            let outcome = match fs::remove_dir_all(path) {
                Ok(()) => Outcome::Deleted,
                Err(error) => Outcome::Failed(format!("could not finish deleting it: {error}")),
            };
            vec![Removal {
                path: path.clone(),
                size: 0,
                outcome,
            }]
        });
        let projects = scan
            .plan
            .projects
            .par_iter()
            .filter(|project| project.reclaimable())
            .filter(|project| {
                let only = options.only.as_ref();
                only.is_none_or(|only| only.contains(&project.path))
            })
            .map(|project| context.clean_project(project));
        interrupted
            .chain(projects)
            .flat_map_iter(|removals| {
                removals.iter().for_each(on_done);
                removals
            })
            .collect()
    };
    Ok(
        match rayon::ThreadPoolBuilder::new().num_threads(WORKERS).build() {
            Ok(pool) => pool.install(work),
            Err(_) => work(),
        },
    )
}

impl Context<'_> {
    fn clean_project(&self, project: &Project) -> Vec<Removal> {
        // The project might have become active since the scan.
        let refusal = match self.scan.reassess(project, &self.git, self.options.now) {
            Warmth::Cold { .. } => None,
            Warmth::Warm { signal, .. } => {
                Some(format!("the project became active: {}", signal.describe()))
            }
            Warmth::Unknown(reason) => Some(reason),
        };
        let open_files = if self.options.check_open_files && refusal.is_none() {
            open_files()
        } else {
            Vec::new()
        };
        project
            .dust
            .iter()
            .map(|dust| {
                let outcome = match &refusal {
                    Some(reason) => Outcome::Skipped(reason.clone()),
                    None => match self.check(dust, &open_files) {
                        Err(reason) => Outcome::Skipped(reason),
                        Ok(()) => self.remove(dust, project),
                    },
                };
                Removal {
                    path: dust.path.clone(),
                    size: dust.size,
                    outcome,
                }
            })
            .collect()
    }

    /// The checks of design §8 that concern a single directory.
    fn check(&self, dust: &Dust, open_files: &[PathBuf]) -> Result<(), String> {
        let Ok(meta) = fs::symlink_metadata(&dust.path) else {
            return Err("it is already gone".into());
        };
        if !meta.is_dir() {
            return Err("it is no longer a directory".into());
        }
        let Some(parent) = dust.path.parent() else {
            return Err("it has no parent folder".into());
        };
        if parent.canonicalize().ok().as_deref() != Some(parent) {
            return Err("its path now passes through a symlink".into());
        }
        if fs::metadata(parent).map_or(true, |parent| parent.dev() != meta.dev()) {
            return Err("it is a mount point".into());
        }
        if !dust.path.starts_with(&dust.scan_root) || is_forbidden(&dust.path, self.home.as_deref())
        {
            return Err("it is outside the folders that may be cleaned".into());
        }
        classify::recheck(dust, &self.git, &self.scan.rules)?;
        if open_files.iter().any(|file| file.starts_with(&dust.path)) {
            return Err("a running process has files open in it".into());
        }
        Ok(())
    }

    fn remove(&self, dust: &Dust, project: &Project) -> Outcome {
        if self.options.trash {
            // No rename first: the Trash keeps the original name, so "Put
            // Back" restores `node_modules`, and moving there is quick.
            if let Err(error) = trash::delete(&dust.path) {
                return Outcome::Failed(format!("could not move it to the Trash: {error}"));
            }
            return match self.log(dust, project, "trashed") {
                Ok(()) => Outcome::Trashed,
                Err(error) => Outcome::Failed(format!("trashed, but not logged: {error}")),
            };
        }

        // Renaming is atomic and instant, so a killed run never leaves a
        // half-deleted directory under a name the project still uses.
        let id = RandomState::new().build_hasher().finish();
        let holding = dust
            .path
            .with_file_name(format!("{INTERRUPTED_PREFIX}{id:016x}"));
        if let Err(error) = fs::rename(&dust.path, &holding) {
            return Outcome::Failed(format!("could not move it aside: {error}"));
        }
        // Logged here, not after the delete: from this point the directory is
        // gone as far as the project is concerned, even if the run is killed.
        let logged = self.log(dust, project, "deleted");
        if let Err(error) = fs::remove_dir_all(&holding) {
            return Outcome::Failed(format!(
                "partly deleted; the rest is in {} and the next clean --apply retries it: {error}",
                holding.display()
            ));
        }
        match logged {
            Ok(()) => Outcome::Deleted,
            Err(error) => Outcome::Failed(format!("deleted, but not logged: {error}")),
        }
    }

    fn log(&self, dust: &Dust, project: &Project, action: &str) -> std::io::Result<()> {
        let entry = LogEntry {
            time: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
            action,
            path: &dust.path,
            project: &project.path,
            rule: dust.rule,
            size: dust.size,
            restore_hint: &dust.restore_hint,
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        self.log.lock().unwrap().write_all(line.as_bytes())
    }
}

/// Paths that are never deleted, whatever the roots say: `/`, system
/// folders, the home folder and anything above it, and `~/Library`.
fn is_forbidden(path: &Path, home: Option<&Path>) -> bool {
    const SYSTEM: &[&str] = &[
        "/System",
        "/Library",
        "/Applications",
        "/usr",
        "/bin",
        "/sbin",
        "/etc",
        "/private/etc",
        "/dev",
        "/proc",
        "/sys",
        "/boot",
        "/lib",
        "/lib64",
    ];
    path.parent().is_none()
        || SYSTEM.iter().any(|folder| path.starts_with(folder))
        || home.is_some_and(|home| home.starts_with(path) || path.starts_with(home.join("Library")))
}

/// Every file that some process of this user has open, from one `lsof` run.
/// Best effort: empty when `lsof` is missing or takes too long.
///
/// `-n -P` matter: without them lsof resolves host and port names for every
/// socket and takes seconds instead of a fraction of one. One listing of
/// everything is also far quicker than `lsof +D`, which walks the directory.
fn open_files() -> Vec<PathBuf> {
    const LIMIT: Duration = Duration::from_secs(5);
    let Ok(mut child) = Command::new("lsof")
        .args(["-w", "-n", "-P", "-Fn"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // `-Fn` prints one field per line; names start with `n`.
        let files: Vec<PathBuf> = stdout
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| line.strip_prefix('n').map(PathBuf::from))
            .collect();
        let _ = sender.send(files);
    });
    let files = receiver.recv_timeout(LIMIT).unwrap_or_default();
    let _ = child.kill();
    let _ = child.wait();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_deletion_names() {
        assert!(is_interrupted_deletion(OsStr::new(
            ".devdust-deleting-00ff00ff00ff00ff"
        )));
        assert!(!is_interrupted_deletion(OsStr::new(".devdust-deleting-")));
        assert!(!is_interrupted_deletion(OsStr::new(
            ".devdust-deleting-not-hex-not-hex!"
        )));
        assert!(!is_interrupted_deletion(OsStr::new("node_modules")));
    }

    #[test]
    fn forbidden_paths() {
        let home = Some(Path::new("/Users/me"));
        for path in [
            "/",
            "/Users",
            "/Users/me",
            "/Users/me/Library/Caches/target",
            "/usr/local/lib/node_modules",
            "/System/Library/build",
            "/private/etc/vendor",
        ] {
            assert!(is_forbidden(Path::new(path), home), "{path}");
        }
        for path in [
            "/Users/me/Code/app/node_modules",
            "/Users/meow/target",
            "/private/var/folders/x/app/target",
            "/Volumes/Work/app/build",
        ] {
            assert!(!is_forbidden(Path::new(path), home), "{path}");
        }
    }
}
