//! Shared data types.

use std::path::PathBuf;

use serde::Serialize;

use crate::rules::Rule;

/// A directory whose name and sibling marker match a dust rule. The rule's
/// extra conditions have not been checked yet.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub rule: &'static Rule,
    /// The entry of `rule.dirs` that matched.
    pub name: &'static str,
    /// Folder the rule's directory is relative to; holds the marker if the rule needs one.
    pub marker_dir: PathBuf,
    /// The scan root this candidate was found under.
    pub root: PathBuf,
}

/// A candidate that passed every check of its rule.
#[derive(Debug, Clone, Serialize)]
pub struct Dust {
    pub path: PathBuf,
    /// The rule's directory pattern that matched, such as `node_modules`.
    pub name: &'static str,
    pub rule: &'static str,
    #[serde(skip)]
    pub definition: &'static Rule,
    pub ecosystem: &'static str,
    #[serde(skip)]
    pub project_root: PathBuf,
    /// Folder the rule's directory is relative to; holds the marker if the rule needs one.
    #[serde(skip)]
    pub marker_dir: PathBuf,
    /// Repo used for tracked-file and git-ignore checks: the submodule if the
    /// dust is inside one, else the worktree. `None` outside git.
    #[serde(skip)]
    pub git_scope: Option<PathBuf>,
    /// The scan root it was found under.
    #[serde(skip)]
    pub scan_root: PathBuf,
    pub restore_hint: String,
    /// Allocated bytes, counting each hardlinked file once within this directory.
    pub size: u64,
}

/// A candidate that was rejected, and why.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub path: PathBuf,
    pub rule: &'static str,
    pub reason: String,
}
