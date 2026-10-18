//! The final list: dust grouped by unit, each with its verdict and reason.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::assess::{Signal, Unit, Warmth};
use crate::measure::Measurement;
use crate::model::{Dust, Skipped};
use crate::worktree::Worktrees;

#[derive(Debug, Serialize)]
pub struct Plan {
    /// Reclaimable projects first, then the kept ones; largest first in each.
    pub projects: Vec<Project>,
    pub skipped: Vec<Skipped>,
    /// Dust that an interrupted `clean` renamed but did not finish deleting.
    /// The next `clean --apply` finishes the job.
    pub interrupted: Vec<PathBuf>,
    /// Folders that hold a `.devdustkeep` file and were not searched.
    pub opted_out: Vec<PathBuf>,
    pub totals: Totals,
}

/// Which held projects `clean` may touch anyway.
#[derive(Debug, Clone, Copy, Default)]
pub struct Include {
    pub locked: bool,
    pub external_worktrees: bool,
}

/// One unit of the cold decision: a workspace, a project, or a worktree.
#[derive(Debug, Serialize)]
pub struct Project {
    pub path: PathBuf,
    /// Root of the git worktree it belongs to.
    pub worktree: Option<PathBuf>,
    /// `None` outside git, and when `HEAD` is detached.
    pub branch: Option<String>,
    pub state: State,
    /// The signal that makes it warm, or the newest signal of a cold project.
    pub signal: Option<Signal>,
    /// Unix time of `signal`.
    pub signal_at: Option<u64>,
    /// Seconds since `signal_at`.
    pub age_seconds: Option<u64>,
    /// Why a cold project is not reclaimable, or why the state is unknown.
    pub held: Option<String>,
    pub size: u64,
    /// Largest first.
    pub dust: Vec<Dust>,
    /// Index of the unit it was built from.
    #[serde(skip)]
    pub(crate) unit: usize,
}

impl Project {
    /// Cold, and not held back by a lock or by lying outside the roots.
    pub fn reclaimable(&self) -> bool {
        self.state == State::Cold && self.held.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Cold,
    Warm,
    Unknown,
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub reclaimable: Sum,
    pub kept: Sum,
    /// Projects left out because they are smaller than the minimum size.
    pub below_min_size: usize,
    /// Directories and files that could not be read.
    pub unreadable: u64,
}

#[derive(Debug, Serialize)]
pub struct Sum {
    pub projects: usize,
    pub dust_dirs: usize,
    /// Allocated bytes, each hardlinked file counted once.
    pub apparent_bytes: u64,
    /// Bytes that deleting this dust would actually free.
    pub would_free_bytes: u64,
}

/// `dust` must be in the order it was measured in and carry its sizes, and
/// `warmth` must be in the order of `units`.
#[allow(clippy::too_many_arguments)]
pub fn build(
    dust: &[Dust],
    skipped: Vec<Skipped>,
    interrupted: Vec<PathBuf>,
    opted_out: Vec<PathBuf>,
    measurement: &Measurement,
    units: &[Unit],
    warmth: Vec<Warmth>,
    worktrees: &Worktrees,
    include: Include,
    min_size: u64,
    now: SystemTime,
    unreadable: u64,
) -> Plan {
    let mut reclaimed = vec![false; dust.len()];
    let mut kept = vec![false; dust.len()];
    let mut projects = Vec::new();
    let mut below_min_size = 0;
    for (unit_index, (unit, warmth)) in units.iter().zip(warmth).enumerate() {
        let size: u64 = unit.dust.iter().map(|&index| dust[index].size).sum();
        if size < min_size {
            below_min_size += 1;
            continue;
        }
        let worktree = unit.worktree.map(|at| &worktrees.list[at]);
        let (state, signal, mut held) = match warmth {
            Warmth::Warm { signal, at } => (State::Warm, Some((signal, at)), None),
            Warmth::Cold { newest } => (State::Cold, newest, None),
            Warmth::Unknown(reason) => (State::Unknown, None, Some(reason)),
        };
        if state == State::Cold
            && let Some(worktree) = worktree
        {
            if worktree.locked && !include.locked {
                held = Some("locked worktree".into());
            } else if !worktree.inside_roots && !include.external_worktrees {
                held = Some("worktree outside the roots".into());
            }
        }

        let mut items: Vec<Dust> = unit.dust.iter().map(|&i| dust[i].clone()).collect();
        items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
        let project = Project {
            path: unit.root.clone(),
            worktree: worktree.map(|w| w.root.clone()),
            branch: worktree.and_then(|w| w.branch.clone()),
            state,
            signal: signal.map(|(signal, _)| signal),
            signal_at: signal
                .and_then(|(_, at)| Some(at.duration_since(UNIX_EPOCH).ok()?.as_secs())),
            age_seconds: signal
                .map(|(_, at)| now.duration_since(at).map_or(0, |age| age.as_secs())),
            held,
            size,
            dust: items,
            unit: unit_index,
        };
        let chosen = if project.reclaimable() {
            &mut reclaimed
        } else {
            &mut kept
        };
        for &index in &unit.dust {
            chosen[index] = true;
        }
        projects.push(project);
    }
    projects.sort_by(|a, b| {
        (b.reclaimable().cmp(&a.reclaimable()))
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.path.cmp(&b.path))
    });

    let sum = |chosen: &[bool], reclaimable: bool| {
        let mut listed = projects.iter().filter(|p| p.reclaimable() == reclaimable);
        Sum {
            projects: listed.clone().count(),
            dust_dirs: listed.by_ref().map(|p| p.dust.len()).sum(),
            apparent_bytes: measurement.apparent(chosen),
            would_free_bytes: measurement.would_free(chosen),
        }
    };
    Plan {
        totals: Totals {
            reclaimable: sum(&reclaimed, true),
            kept: sum(&kept, false),
            below_min_size,
            unreadable: unreadable + measurement.unreadable,
        },
        projects,
        skipped,
        interrupted,
        opted_out,
    }
}
