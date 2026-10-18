//! Checks each candidate against its rule's extra conditions.

use std::path::{Path, PathBuf};

use dashmap::DashMap;
use rayon::prelude::*;

use crate::git::Git;
use crate::model::{Candidate, Dust, Skipped};
use crate::rules::{self, RuleSet};
use crate::worktree::git_scope;

/// Splits candidates into dust and rejected candidates. Both lists keep the
/// order of `candidates`.
pub fn classify(
    candidates: Vec<Candidate>,
    git: &Git,
    rules: &RuleSet,
) -> (Vec<Dust>, Vec<Skipped>) {
    let seen = Seen {
        rules,
        manifests: DashMap::new(),
        keeps: DashMap::new(),
    };
    let results: Vec<_> = candidates
        .into_par_iter()
        .map(|candidate| {
            let (path, rule) = (candidate.path.clone(), candidate.rule.id);
            check(candidate, git, &seen).map_err(|reason| Skipped { path, rule, reason })
        })
        .collect();

    let mut dust = Vec::new();
    let mut skipped = Vec::new();
    for result in results {
        match result {
            Ok(found) => dust.push(found),
            Err(rejected) => skipped.push(rejected),
        }
    }
    (dust, skipped)
}

/// What folders were found to hold, so shared ancestors are read once.
struct Seen<'a> {
    rules: &'a RuleSet,
    /// Folder → holds a manifest.
    manifests: DashMap<PathBuf, bool>,
    /// Folder → the manifest in it that opts out of cleaning.
    keeps: DashMap<PathBuf, Option<PathBuf>>,
}

fn check(candidate: Candidate, git: &Git, seen: &Seen) -> Result<Dust, String> {
    let Candidate {
        path,
        rule,
        name,
        marker_dir,
        root,
    } = candidate;

    if rule.requires_flutter && !rules::depends_on_flutter(&marker_dir) {
        return Err("pubspec.yaml does not depend on Flutter".into());
    }

    let git_scope = git_scope(&path);
    // A git failure means skip and report, never "assume it is fine".
    let failed = |error| format!("skipped because a git check failed: {error}");
    if let Some(scope) = &git_scope
        && git.has_tracked_files(scope, &path).map_err(failed)?
    {
        return Err("contains files tracked by git".into());
    }
    if rule.requires_ignored {
        let Some(scope) = &git_scope else {
            return Err("not in a git repository, so it cannot be confirmed as git-ignored".into());
        };
        if !git.is_ignored(scope, &path).map_err(failed)? {
            return Err("not git-ignored".into());
        }
    }

    let boundary = git_scope.as_deref().unwrap_or(&root);
    // A manifest can opt its project out, and a workspace root can do so
    // for all of its packages.
    for folder in marker_dir.ancestors() {
        let keep = seen
            .keeps
            .entry(folder.to_path_buf())
            .or_insert_with(|| rules::keep_manifest(folder))
            .clone();
        if let Some(manifest) = keep {
            return Err(format!("kept: {} sets devdust.keep", manifest.display()));
        }
        if folder == boundary || !folder.starts_with(boundary) {
            break;
        }
    }
    let project_root = if rule.markers.is_empty() {
        // Outside git and with no manifest above it, the best guess is the
        // top-level folder under the scan root: the root itself would lump
        // strangers together, and the cache's own folder would split one
        // project (or a marker-less virtualenv) into hundreds.
        let top_level = marker_dir
            .strip_prefix(&root)
            .ok()
            .and_then(|below| below.components().next())
            .map_or(root.clone(), |first| root.join(first));
        let fallback = git_scope.as_deref().unwrap_or(&top_level);
        nearest_manifest_dir(&marker_dir, boundary, fallback, seen)
    } else {
        marker_dir.clone()
    };
    Ok(Dust {
        restore_hint: rules::restore_hint(rule, &marker_dir, boundary),
        path,
        name,
        rule: rule.id,
        definition: rule,
        ecosystem: rule.ecosystem,
        project_root,
        marker_dir,
        git_scope,
        scan_root: root,
        size: 0,
    })
}

/// Runs a dust directory's checks again, right before it is deleted: its
/// marker, the rule's conditions, and that git tracks nothing inside it.
pub(crate) fn recheck(dust: &Dust, git: &Git, rules: &RuleSet) -> Result<(), String> {
    let rule = dust.definition;
    if !rule.markers.is_empty()
        && !rules::has_marker(rule.markers, &rules::file_names(&dust.marker_dir))
    {
        return Err("its manifest is gone".into());
    }
    let candidate = Candidate {
        path: dust.path.clone(),
        rule,
        name: dust.name,
        marker_dir: dust.marker_dir.clone(),
        root: dust.scan_root.clone(),
    };
    let seen = Seen {
        rules,
        manifests: DashMap::new(),
        keeps: DashMap::new(),
    };
    check(candidate, git, &seen).map(|_| ())
}

/// The project a marker-less dust directory in `dir` belongs to: the nearest
/// folder holding any manifest, looking no higher than `boundary`. Without a
/// manifest it is `fallback`.
fn nearest_manifest_dir(dir: &Path, boundary: &Path, fallback: &Path, seen: &Seen) -> PathBuf {
    if !dir.starts_with(boundary) {
        return dir.to_path_buf();
    }
    for ancestor in dir.ancestors() {
        let has_manifest = *seen
            .manifests
            .entry(ancestor.to_path_buf())
            .or_insert_with(|| seen.rules.has_manifest(&rules::file_names(ancestor)));
        if has_manifest {
            return ancestor.to_path_buf();
        }
        if ancestor == boundary {
            break;
        }
    }
    fallback.to_path_buf()
}
