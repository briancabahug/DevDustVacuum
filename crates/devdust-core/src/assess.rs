//! The warm/cold decision (design §5, §13, §14).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use serde::Serialize;
use walkdir::WalkDir;

use crate::execute;
use crate::git::Git;
use crate::model::Dust;
use crate::rules::{self, RuleSet};
use crate::worktree::{self, Worktrees};

/// Overrides the default grouping, which is "a declared workspace, else the
/// outermost project".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    /// One unit per worktree.
    Repo,
    /// One unit per project, even inside a workspace or another project.
    Project,
}

/// What the cold decision applies to: a workspace, a project, or a worktree.
#[derive(Debug)]
pub struct Unit {
    pub root: PathBuf,
    pub worktree: Option<usize>,
    /// `root` is the worktree root, so the reflog applies. Otherwise only
    /// signals scoped to `root` are used.
    pub covers_worktree: bool,
    /// Indexes of the unit's dust directories.
    pub dust: Vec<usize>,
    /// Why the unit cannot be assessed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// Newest entry of the worktree's HEAD reflog: a commit, checkout, reset, or rebase.
    Reflog,
    /// Committer date of `HEAD`, when there is no reflog.
    HeadCommit,
    /// Newest commit on `HEAD` that touches the unit's path.
    PathCommit,
    UncommittedChanges,
    SourceModified,
    DustModified,
}

impl Signal {
    /// The activity the signal stands for, as a noun phrase.
    pub fn describe(self) -> &'static str {
        match self {
            Signal::Reflog => "a commit or checkout",
            Signal::HeadCommit | Signal::PathCommit => "a commit",
            Signal::UncommittedChanges => "uncommitted changes",
            Signal::SourceModified => "an edited file",
            Signal::DustModified => "rebuilt dust",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warmth {
    Warm {
        signal: Signal,
        at: SystemTime,
    },
    Cold {
        newest: Option<(Signal, SystemTime)>,
    },
    /// A git check failed. Never treated as cold.
    Unknown(String),
}

/// Groups the dust into units, in path order.
pub fn group(
    dust: &[Dust],
    worktrees: &Worktrees,
    group_by: Option<GroupBy>,
    rules: &RuleSet,
) -> Vec<Unit> {
    let mut folders = Folders {
        rules,
        seen: HashMap::new(),
    };
    let mut units: BTreeMap<PathBuf, Unit> = BTreeMap::new();
    for (index, item) in dust.iter().enumerate() {
        let (root, worktree, error) = match worktrees.of(item) {
            Err(reason) => (item.project_root.clone(), None, Some(reason.to_string())),
            Ok(worktree) => {
                let worktree_root = worktree.map(|at| worktrees.list[at].root.as_path());
                let root = match group_by {
                    Some(GroupBy::Repo) => worktree_root.unwrap_or(&item.project_root).into(),
                    Some(GroupBy::Project) => item.project_root.clone(),
                    None => folders
                        .unit_root(&item.project_root, worktree_root.unwrap_or(&item.scan_root)),
                };
                (root, worktree, None)
            }
        };
        let unit = units.entry(root).or_insert_with_key(|root| Unit {
            root: root.clone(),
            worktree,
            covers_worktree: worktree.is_some_and(|at| worktrees.list[at].root == *root),
            dust: Vec::new(),
            error,
        });
        unit.dust.push(index);
    }
    units.into_values().collect()
}

/// Remembers what each folder declares, so shared ancestors are read once.
struct Folders<'a> {
    rules: &'a RuleSet,
    /// Folder → (declares a workspace, holds a manifest).
    seen: HashMap<PathBuf, (bool, bool)>,
}

impl Folders<'_> {
    /// The unit a project belongs to, looking no higher than `bound`: the
    /// nearest folder that declares a workspace, else the outermost folder
    /// that holds a manifest, so that a project nested in another (`ios/`
    /// with its `Podfile` inside a Flutter app) is judged with its parent.
    fn unit_root(&mut self, project: &Path, bound: &Path) -> PathBuf {
        if !project.starts_with(bound) {
            return project.to_path_buf();
        }
        let mut chain = Vec::new();
        for folder in project.ancestors() {
            chain.push(folder);
            if folder == bound {
                break;
            }
        }
        if let Some(workspace) = chain.iter().find(|folder| self.look(folder).0) {
            return workspace.to_path_buf();
        }
        match chain.iter().rev().find(|folder| self.look(folder).1) {
            Some(outermost) => outermost.to_path_buf(),
            None => project.to_path_buf(),
        }
    }

    fn look(&mut self, folder: &Path) -> (bool, bool) {
        *self.seen.entry(folder.to_path_buf()).or_insert_with(|| {
            let names = rules::file_names(folder);
            (
                declares_workspace(folder, &names),
                self.rules.has_manifest(&names),
            )
        })
    }
}

fn declares_workspace(folder: &Path, names: &[String]) -> bool {
    let has = |name: &str| names.iter().any(|n| n == name);
    let read = |name: &str| fs::read_to_string(folder.join(name)).unwrap_or_default();
    has("pnpm-workspace.yaml")
        || has("go.work")
        || (has("package.json")
            && serde_json::from_str::<serde_json::Value>(&read("package.json"))
                .is_ok_and(|manifest| manifest.get("workspaces").is_some()))
        || (has("Cargo.toml")
            && read("Cargo.toml").lines().any(|line| {
                let line = line.trim();
                line.starts_with("[workspace]") || line.starts_with("[workspace.")
            }))
}

/// Decides each unit's warmth. `now` is a parameter so tests are deterministic.
pub fn assess(
    units: &[Unit],
    dust: &[Dust],
    worktrees: &Worktrees,
    git: &Git,
    now: SystemTime,
    older_than: Duration,
) -> Vec<Warmth> {
    let dust_paths: HashSet<&Path> = dust.iter().map(|d| d.path.as_path()).collect();
    units
        .par_iter()
        .map(|unit| assess_one(unit, dust, &dust_paths, worktrees, git, now, older_than))
        .collect()
}

/// Decides one unit's warmth. `dust_paths` holds every dust directory of the scan.
pub fn assess_one(
    unit: &Unit,
    dust: &[Dust],
    dust_paths: &HashSet<&Path>,
    worktrees: &Worktrees,
    git: &Git,
    now: SystemTime,
    older_than: Duration,
) -> Warmth {
    let mut tally = Tally {
        now,
        older_than,
        newest: None,
    };
    match assess_unit(unit, dust, dust_paths, worktrees, git, &mut tally) {
        Ok(Some(warm)) => warm,
        Ok(None) => Warmth::Cold {
            newest: tally.newest,
        },
        Err(reason) => Warmth::Unknown(reason),
    }
}

struct Tally {
    now: SystemTime,
    older_than: Duration,
    newest: Option<(Signal, SystemTime)>,
}

impl Tally {
    /// A timestamp in the future counts as recent.
    fn is_recent(&self, at: SystemTime) -> bool {
        self.now
            .duration_since(at)
            .map_or(true, |age| age < self.older_than)
    }

    /// Records a signal. Returns the verdict if the signal makes the unit warm.
    fn see(&mut self, signal: Signal, at: SystemTime) -> Option<Warmth> {
        if self.is_recent(at) {
            return Some(Warmth::Warm { signal, at });
        }
        if self.newest.is_none_or(|(_, newest)| at > newest) {
            self.newest = Some((signal, at));
        }
        None
    }
}

/// Checks the signals from cheapest to most expensive and stops at the first
/// recent one. `Ok(None)` means none was recent.
fn assess_unit(
    unit: &Unit,
    dust: &[Dust],
    dust_paths: &HashSet<&Path>,
    worktrees: &Worktrees,
    git: &Git,
    tally: &mut Tally,
) -> Result<Option<Warmth>, String> {
    if let Some(error) = &unit.error {
        return Err(error.clone());
    }
    let worktree = unit.worktree.map(|at| &worktrees.list[at]);

    // The reflog is worktree-wide: for a unit that is only part of a
    // worktree, one checkout anywhere would keep it warm.
    let reflog = worktree
        .filter(|_| unit.covers_worktree)
        .and_then(|worktree| reflog_time(&worktree.git_dir));
    if let Some(at) = reflog
        && let Some(warm) = tally.see(Signal::Reflog, at)
    {
        return Ok(Some(warm));
    }

    for &index in &unit.dust {
        if let Some(at) = dust_mtime(&dust[index].path)
            && let Some(warm) = tally.see(Signal::DustModified, at)
        {
            return Ok(Some(warm));
        }
    }

    if let Some(at) = newest_source(&unit.root, dust_paths, |at| tally.is_recent(at))
        && let Some(warm) = tally.see(Signal::SourceModified, at)
    {
        return Ok(Some(warm));
    }

    let Some(worktree) = worktree else {
        return Ok(None);
    };
    let failed = |error| format!("skipped because a git check failed: {error}");
    let commit = if !unit.covers_worktree {
        let at = git.last_commit(&worktree.root, Some(&unit.root));
        at.map_err(failed)?.map(|at| (Signal::PathCommit, at))
    } else if reflog.is_none() {
        let at = git.last_commit(&worktree.root, None);
        at.map_err(failed)?.map(|at| (Signal::HeadCommit, at))
    } else {
        None
    };
    if let Some((signal, at)) = commit
        && let Some(warm) = tally.see(signal, at)
    {
        return Ok(Some(warm));
    }

    let dust_inside = dust_paths
        .iter()
        .filter(|path| path.starts_with(&unit.root))
        .filter_map(|path| path.strip_prefix(&worktree.root).ok())
        .map(Path::to_path_buf)
        .collect();
    let changed = git.has_changes(&worktree.root, &unit.root, dust_inside);
    if changed.map_err(failed)? {
        // Uncommitted work has no date; it keeps the unit warm whatever its age.
        return Ok(Some(Warmth::Warm {
            signal: Signal::UncommittedChanges,
            at: tally.now,
        }));
    }
    Ok(None)
}

/// Time of the newest entry in a worktree's HEAD reflog, read straight from
/// `logs/HEAD`: much faster than starting git, and the format is stable.
fn reflog_time(git_dir: &Path) -> Option<SystemTime> {
    // Entries are appended, so the newest is the last line. A line is
    // `<old> <new> <name> <email> <seconds> <zone>\t<message>`.
    const TAIL: u64 = 64 * 1024;
    let mut file = File::open(git_dir.join("logs/HEAD")).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    parse_reflog_line(tail.lines().rfind(|line| !line.trim().is_empty())?)
}

fn parse_reflog_line(line: &str) -> Option<SystemTime> {
    let (identity, _message) = line.split_once('\t').unwrap_or((line, ""));
    let seconds = identity.split_whitespace().rev().nth(1)?.parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// Newest mtime of a dust directory and its direct children. The children
/// matter because a rebuild often rewrites `target/debug` or `.next/cache`
/// without touching the top directory.
fn dust_mtime(dir: &Path) -> Option<SystemTime> {
    let mtime = |path: &Path| fs::symlink_metadata(path).and_then(|m| m.modified()).ok();
    let children = fs::read_dir(dir).into_iter().flatten().flatten();
    children
        .filter_map(|child| mtime(&child.path()))
        .chain(mtime(dir))
        .max()
}

/// Newest mtime of the files under `root`, not counting dust directories,
/// `.git`, half-deleted dust, and worktrees nested inside. Returns early with the first file
/// that `is_recent`.
fn newest_source(
    root: &Path,
    dust_paths: &HashSet<&Path>,
    is_recent: impl Fn(SystemTime) -> bool,
) -> Option<SystemTime> {
    let walk = WalkDir::new(root).into_iter().filter_entry(|entry| {
        if !entry.file_type().is_dir() {
            // Finder rewrites this whenever the folder is merely looked at.
            return entry.file_name() != ".DS_Store";
        }
        entry.file_name() != ".git"
            && !execute::is_interrupted_deletion(entry.file_name())
            && !dust_paths.contains(entry.path())
            && (entry.depth() == 0 || !worktree::is_worktree_root(entry.path()))
    });
    let mut newest = None;
    for entry in walk.flatten() {
        if entry.file_type().is_dir() {
            continue;
        }
        let Some(at) = entry.metadata().ok().and_then(|m| m.modified().ok()) else {
            continue;
        };
        if is_recent(at) {
            return Some(at);
        }
        newest = newest.max(Some(at));
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflog_lines() {
        let at = |seconds| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        let line = "0000 1111 Ada Lovelace <ada@example.com> 1700000000 +0200\tcommit: tab\there";
        assert_eq!(parse_reflog_line(line), at(1700000000));
        // No message, and a name made of several words.
        let line = "0000 1111 A B C <a@example.com> 1600000000 -0700";
        assert_eq!(parse_reflog_line(line), at(1600000000));
        assert_eq!(parse_reflog_line("garbage"), None);
    }
}
