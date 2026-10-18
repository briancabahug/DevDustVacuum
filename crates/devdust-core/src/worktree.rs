//! Git worktrees: which one each dust directory belongs to (design §14).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::git::{Git, GitError};
use crate::model::Dust;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeKind {
    Main,
    Linked,
}

#[derive(Debug, Clone)]
pub struct Worktree {
    pub root: PathBuf,
    /// Per-worktree dir; holds `HEAD` and `logs/HEAD`.
    pub git_dir: PathBuf,
    /// Shared repo dir.
    pub common_dir: PathBuf,
    pub kind: WorktreeKind,
    /// `None` when `HEAD` is detached.
    pub branch: Option<String>,
    pub locked: bool,
    /// Whether it overlaps a configured root, as opposed to a linked worktree
    /// that was only found through its repo.
    pub inside_roots: bool,
}

/// The worktrees that hold dust.
pub struct Worktrees {
    pub list: Vec<Worktree>,
    /// Git scope of a dust directory → index into `list`, or why it is unknown.
    of_scope: BTreeMap<PathBuf, Result<usize, String>>,
}

impl Worktrees {
    /// Works out the worktree of every dust directory. A dust directory in a
    /// submodule belongs to the worktree that contains the submodule.
    pub fn resolve(dust: &[Dust], roots: &[PathBuf], git: &Git) -> Self {
        let scopes: BTreeSet<&Path> = dust.iter().filter_map(|d| d.git_scope.as_deref()).collect();
        let located: Vec<_> = scopes
            .into_par_iter()
            .map(|scope| (scope, locate(scope, roots, git)))
            .collect();

        let mut list: Vec<Worktree> = Vec::new();
        let mut of_scope = BTreeMap::new();
        for (scope, worktree) in located {
            let index = worktree
                .map(|worktree| {
                    list.iter()
                        .position(|known| known.root == worktree.root)
                        .unwrap_or_else(|| {
                            list.push(worktree);
                            list.len() - 1
                        })
                })
                .map_err(|error| format!("skipped because a git check failed: {error}"));
            of_scope.insert(scope.to_path_buf(), index);
        }
        Self { list, of_scope }
    }

    /// The worktree of a dust directory: `None` outside git, `Err` when git
    /// could not tell.
    pub fn of(&self, dust: &Dust) -> Result<Option<usize>, &str> {
        match &dust.git_scope {
            None => Ok(None),
            Some(scope) => match &self.of_scope[scope] {
                Ok(index) => Ok(Some(*index)),
                Err(reason) => Err(reason),
            },
        }
    }
}

fn locate(scope: &Path, roots: &[PathBuf], git: &Git) -> Result<Worktree, GitError> {
    let mut root = scope.to_path_buf();
    loop {
        let (git_dir, common_dir) = git.git_dirs(&root)?;
        if is_submodule(&root, &git_dir, &common_dir)
            && let Some(parent) = git_scope(&root)
        {
            root = parent;
            continue;
        }
        let kind = if git_dir == common_dir {
            WorktreeKind::Main
        } else {
            WorktreeKind::Linked
        };
        return Ok(Worktree {
            branch: branch(&git_dir),
            // `git worktree lock` leaves this file in the worktree's git dir.
            locked: kind == WorktreeKind::Linked && git_dir.join("locked").exists(),
            inside_roots: roots
                .iter()
                .any(|r| root.starts_with(r) || r.starts_with(&root)),
            root,
            git_dir,
            common_dir,
            kind,
        });
    }
}

/// A submodule has a `.git` file like a linked worktree, but its git dir is
/// the repo itself, kept under the parent's `.git/modules/`.
fn is_submodule(root: &Path, git_dir: &Path, common_dir: &Path) -> bool {
    git_dir == common_dir
        && root.join(".git").is_file()
        && git_dir
            .components()
            .zip(git_dir.components().skip(1))
            .any(|(a, b)| a.as_os_str() == ".git" && b.as_os_str() == "modules")
}

fn branch(git_dir: &Path) -> Option<String> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let name = head.trim().strip_prefix("ref: refs/heads/")?;
    Some(name.to_string())
}

/// The nearest ancestor of `path` that holds a `.git` file or directory: a
/// worktree, or a submodule.
pub fn git_scope(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())
        .map(Path::to_path_buf)
}

/// Whether `dir` is the root of a worktree of its own, so its files are not
/// part of the worktree around it. A submodule is not.
pub fn is_worktree_root(dir: &Path) -> bool {
    let git = dir.join(".git");
    match git.symlink_metadata() {
        Err(_) => false,
        Ok(meta) if meta.is_dir() => true,
        Ok(_) => !fs::read_to_string(git).is_ok_and(|text| text.contains("/modules/")),
    }
}

/// Linked worktrees of the repos in `repos` (folders with a `.git` directory)
/// that lie outside every root. Read from `.git/worktrees/*/gitdir`, which
/// holds the path of each linked worktree's `.git` file.
pub fn external_worktrees(repos: &[PathBuf], roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for repo in repos {
        let Ok(entries) = fs::read_dir(repo.join(".git/worktrees")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(gitdir) = fs::read_to_string(entry.path().join("gitdir")) else {
                continue;
            };
            // A worktree whose folder is gone is "prunable": nothing to scan.
            let Some(root) = Path::new(gitdir.trim())
                .parent()
                .and_then(|root| root.canonicalize().ok())
            else {
                continue;
            };
            if root.is_dir() && !roots.iter().any(|r| root.starts_with(r)) {
                found.push(root);
            }
        }
    }
    crate::drop_nested(found)
}
