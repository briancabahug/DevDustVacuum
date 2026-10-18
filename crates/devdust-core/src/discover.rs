//! Parallel walk of the roots that collects dust candidates.

use std::cell::OnceCell;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use globset::GlobSet;
use jwalk::WalkDirGeneric;

use crate::execute;
use crate::model::Candidate;
use crate::rules::{self, Rule, RuleSet};

/// Dependency stores that are not searched even when they are not dust (no
/// manifest beside them): the packages inside would look like projects.
const NEVER_SEARCHED: &[&str] = &["node_modules", "site-packages"];

pub struct Discovery {
    pub candidates: Vec<Candidate>,
    /// Folders that hold a `.git` directory: main worktrees.
    pub repos: Vec<PathBuf>,
    /// Dust that an interrupted `clean` renamed but did not finish deleting.
    pub interrupted: Vec<PathBuf>,
    /// Folders that hold a `.devdustkeep` file and were therefore not searched.
    pub opted_out: Vec<PathBuf>,
    /// Directories that could not be read.
    pub unreadable: u64,
}

/// Walks `roots` and records every directory that matches a rule's name and
/// marker. Matched directories are not descended into, and neither are `.git`
/// directories, symlinks, virtualenvs, marker-less `node_modules`, folders
/// holding `.devdustkeep`, paths matching `ignore`, or (unless `cross_devices`)
/// other filesystems.
pub fn discover(
    roots: &[PathBuf],
    cross_devices: bool,
    rules: &RuleSet,
    ignore: &GlobSet,
) -> Discovery {
    let found = Arc::new(Mutex::new(Vec::new()));
    let repos = Arc::new(Mutex::new(Vec::new()));
    let interrupted = Arc::new(Mutex::new(Vec::new()));
    let opted_out = Arc::new(Mutex::new(Vec::new()));
    let mut unreadable = 0;

    for root in roots {
        let root_dev = if cross_devices {
            None
        } else {
            fs::metadata(root).ok().map(|m| m.dev())
        };
        let found = Arc::clone(&found);
        let repos = Arc::clone(&repos);
        let interrupted = Arc::clone(&interrupted);
        let opted_out = Arc::clone(&opted_out);
        let (rules, ignore) = (rules.clone(), ignore.clone());
        let scan_root = root.clone();

        let walk = WalkDirGeneric::<((), ())>::new(root)
            .skip_hidden(false)
            .follow_links(false)
            .process_read_dir(move |_depth, dir, _state, children| {
                let siblings = OnceCell::new();
                let mut pruned = Vec::new();
                let mut candidates = Vec::new();
                let holds = |name: &str| {
                    let mut files = children.iter().flatten();
                    files.any(|child| child.file_name == name && !child.file_type.is_dir())
                };
                // The inside of a virtualenv is installed packages, not projects.
                let in_virtualenv = holds("pyvenv.cfg");
                let keep = holds(".devdustkeep");
                if keep {
                    opted_out.lock().unwrap().push(dir.to_path_buf());
                }

                for (index, child) in children.iter().enumerate() {
                    let Ok(child) = child else { continue };
                    if !child.file_type.is_dir() {
                        continue;
                    }
                    if child.file_name == ".git" {
                        repos.lock().unwrap().push(dir.to_path_buf());
                        pruned.push(index);
                        continue;
                    }
                    if let Some(dev) = root_dev
                        && child.metadata().map_or(true, |m| m.dev() != dev)
                    {
                        pruned.push(index);
                        continue;
                    }
                    if execute::is_interrupted_deletion(&child.file_name) {
                        interrupted.lock().unwrap().push(dir.join(&child.file_name));
                        pruned.push(index);
                        continue;
                    }
                    if in_virtualenv || keep || ignore.is_match(dir.join(&child.file_name)) {
                        pruned.push(index);
                        continue;
                    }
                    let matched = match_rule(&rules, dir, &child.file_name, || {
                        siblings.get_or_init(|| {
                            children
                                .iter()
                                .flatten()
                                .filter(|c| !c.file_type.is_dir())
                                .filter_map(|c| c.file_name.to_str().map(String::from))
                                .collect::<Vec<_>>()
                        })
                    });
                    if let Some((rule, name, marker_dir)) = matched {
                        pruned.push(index);
                        candidates.push(Candidate {
                            path: dir.join(&child.file_name),
                            rule,
                            name,
                            marker_dir,
                            root: scan_root.clone(),
                        });
                    } else if NEVER_SEARCHED.iter().any(|name| child.file_name == *name) {
                        pruned.push(index);
                    }
                }

                for index in pruned {
                    if let Ok(child) = &mut children[index] {
                        child.read_children = None;
                    }
                }
                if !candidates.is_empty() {
                    found.lock().unwrap().extend(candidates);
                }
            });

        for entry in walk {
            if entry.is_err() {
                unreadable += 1;
            }
        }
    }

    let mut candidates = std::mem::take(&mut *found.lock().unwrap());
    candidates.sort_by(|a, b| a.path.cmp(&b.path));
    let mut repos = std::mem::take(&mut *repos.lock().unwrap());
    repos.sort();
    let mut interrupted = std::mem::take(&mut *interrupted.lock().unwrap());
    interrupted.sort();
    let mut opted_out = std::mem::take(&mut *opted_out.lock().unwrap());
    opted_out.sort();
    Discovery {
        candidates,
        repos,
        interrupted,
        opted_out,
        unreadable,
    }
}

/// Finds the first rule matching the directory `name` inside `dir`, the
/// pattern of it that matched, and the folder its marker lives in. `siblings` lists the files of `dir`.
fn match_rule<'a>(
    rules: &RuleSet,
    dir: &Path,
    name: &OsStr,
    siblings: impl Fn() -> &'a Vec<String>,
) -> Option<(&'static Rule, &'static str, PathBuf)> {
    let name = name.to_str()?;
    for rule in rules.iter() {
        for pattern in rule.dirs {
            let mut parts = pattern.rsplit('/');
            if parts.next() != Some(name) {
                continue;
            }
            // The rest of a multi-part pattern must match the path leading here.
            let Some(marker_dir) = parts.try_fold(dir, |at, part| {
                (at.file_name() == Some(OsStr::new(part)))
                    .then(|| at.parent())
                    .flatten()
            }) else {
                continue;
            };
            let has_marker = rule.markers.is_empty()
                || if marker_dir == dir {
                    rules::has_marker(rule.markers, siblings())
                } else {
                    rules::has_marker(rule.markers, &rules::file_names(marker_dir))
                };
            if has_marker {
                return Some((rule, pattern, marker_dir.to_path_buf()));
            }
        }
    }
    None
}
