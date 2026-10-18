//! All of devdust's logic. Nothing here prints or exits the process.

pub mod assess;
pub mod classify;
pub mod config;
pub mod discover;
pub mod execute;
pub mod git;
pub mod measure;
pub mod model;
pub mod plan;
pub mod rules;
pub mod schedule;
pub mod worktree;

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use assess::GroupBy;
use assess::{Unit, Warmth};
use globset::GlobSet;
use model::Dust;
pub use plan::{Include, Plan};
use rules::RuleSet;
use worktree::Worktrees;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot scan {}: {source}", path.display())]
    Root { path: PathBuf, source: io::Error },
    #[error("cannot scan {}: not a directory", .0.display())]
    NotADirectory(PathBuf),
    #[error("cannot write the action log {}: {source}", path.display())]
    Log { path: PathBuf, source: io::Error },
    #[error("cannot read the action log {}: {source}", path.display())]
    LogRead { path: PathBuf, source: io::Error },
    #[error("cannot read the config {}: {source}", path.display())]
    ConfigRead { path: PathBuf, source: io::Error },
    #[error("invalid config {}: {reason}", path.display())]
    Config { path: PathBuf, reason: String },
    #[error("{0}")]
    Schedule(String),
}

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    /// Projects with less dust than this are left out of the plan.
    pub min_size: u64,
    pub cross_devices: bool,
    /// A project is cold when every activity signal is at least this old.
    pub older_than: Duration,
    /// `None` groups by declared workspace, else by outermost project.
    pub group_by: Option<GroupBy>,
    /// The time ages are measured from.
    pub now: SystemTime,
    /// Held projects that count as reclaimable anyway.
    pub include: Include,
    pub rules: RuleSet,
    /// Paths that are not searched.
    pub ignore: GlobSet,
}

impl ScanOptions {
    /// The defaults: a 30-day threshold, measured from the current time.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            min_size: 0,
            cross_devices: false,
            older_than: Duration::from_secs(30 * 24 * 60 * 60),
            group_by: None,
            now: SystemTime::now(),
            include: Include::default(),
            rules: RuleSet::default(),
            ignore: GlobSet::empty(),
        }
    }
}

/// Receives progress while a scan runs. Called from several threads.
pub trait Progress: Sync {
    /// Discovery finished and found this many dust directories to measure.
    fn discovered(&self, _dust_dirs: usize) {}
    /// One dust directory of this size was measured.
    fn measured(&self, _bytes: u64) {}
    /// Measuring finished; activity is being checked.
    fn assessing(&self) {}
}

impl Progress for () {}

/// A finished scan: the plan, plus what `execute` needs to check everything
/// again before it deletes.
pub struct Scan {
    pub plan: Plan,
    dust: Vec<Dust>,
    units: Vec<Unit>,
    worktrees: Worktrees,
    older_than: Duration,
    rules: RuleSet,
}

impl Scan {
    /// Assesses the unit of one of the plan's projects again, as of `now`.
    fn reassess(&self, project: &plan::Project, git: &git::Git, now: SystemTime) -> Warmth {
        let dust_paths: HashSet<&Path> = self.dust.iter().map(|d| d.path.as_path()).collect();
        assess::assess_one(
            &self.units[project.unit],
            &self.dust,
            &dust_paths,
            &self.worktrees,
            git,
            now,
            self.older_than,
        )
    }
}

/// Finds and measures the dust under `options.roots` and decides which
/// projects are cold. Read-only.
pub fn scan(options: &ScanOptions, progress: &dyn Progress) -> Result<Scan, Error> {
    let roots = normalize_roots(&options.roots)?;
    let git = git::Git::default();

    let (rules, ignore) = (&options.rules, &options.ignore);
    let mut discovery = discover::discover(&roots, options.cross_devices, rules, ignore);
    // Linked worktrees outside the roots are reported too (design §14).
    let external = worktree::external_worktrees(&discovery.repos, &roots);
    let outside = discover::discover(&external, options.cross_devices, rules, ignore);
    discovery.candidates.extend(outside.candidates);
    discovery.interrupted.extend(outside.interrupted);
    discovery.opted_out.extend(outside.opted_out);
    discovery.candidates.sort_by(|a, b| a.path.cmp(&b.path));
    discovery.candidates.dedup_by(|a, b| a.path == b.path);

    let (mut dust, skipped) = classify::classify(discovery.candidates, &git, rules);
    progress.discovered(dust.len());

    let dirs: Vec<&Path> = dust.iter().map(|d| d.path.as_path()).collect();
    let measurement = measure::measure(&dirs, &|bytes| progress.measured(bytes));
    for (index, item) in dust.iter_mut().enumerate() {
        item.size = measurement.size(index);
    }

    progress.assessing();
    let worktrees = Worktrees::resolve(&dust, &roots, &git);
    let units = assess::group(&dust, &worktrees, options.group_by, rules);
    let warmth = assess::assess(
        &units,
        &dust,
        &worktrees,
        &git,
        options.now,
        options.older_than,
    );
    let plan = plan::build(
        &dust,
        skipped,
        discovery.interrupted,
        discovery.opted_out,
        &measurement,
        &units,
        warmth,
        &worktrees,
        options.include,
        options.min_size,
        options.now,
        discovery.unreadable + outside.unreadable,
    );
    Ok(Scan {
        plan,
        dust,
        units,
        worktrees,
        older_than: options.older_than,
        rules: options.rules.clone(),
    })
}

/// Resolves the roots and drops any that lie inside another, so nothing is
/// walked twice.
fn normalize_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let mut resolved = Vec::new();
    for root in roots {
        let path = root.canonicalize().map_err(|source| Error::Root {
            path: root.clone(),
            source,
        })?;
        if !path.is_dir() {
            return Err(Error::NotADirectory(root.clone()));
        }
        resolved.push(path);
    }
    Ok(drop_nested(resolved))
}

/// Sorts the paths and drops duplicates and any path inside another.
pub(crate) fn drop_nested(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let nested = |path: &PathBuf| paths.iter().any(|p| p != path && path.starts_with(p));
    paths.iter().filter(|path| !nested(path)).cloned().collect()
}
