use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, SystemTime};

use devdust_core::execute::{CleanOptions, Outcome, Removal, execute, read_log};
use devdust_core::{Scan, ScanOptions, scan};
use devdust_fixtures::Fixture;

/// A Node project in its own repo, untouched for 100 days.
fn cold_repo(fx: &Fixture, rel: &str) {
    fx.git_init(rel)
        .file(&format!("{rel}/package.json"), "{}")
        .file(&format!("{rel}/.gitignore"), "node_modules/\n")
        .file(&format!("{rel}/src/index.js"), "")
        .file(&format!("{rel}/node_modules/dep/index.js"), "")
        .commit_all_at(rel, 100)
        .age(rel, 101);
}

fn scan_fixture(fx: &Fixture) -> Scan {
    scan(&ScanOptions::new(vec![fx.root().to_path_buf()]), &()).unwrap()
}

/// A scan in which nothing counts as recent, so that a test can tamper with
/// a project afterwards and still reach the per-directory checks.
fn scan_ignoring_age(fx: &Fixture) -> Scan {
    let mut options = ScanOptions::new(vec![fx.root().to_path_buf()]);
    options.older_than = Duration::ZERO;
    scan(&options, &()).unwrap()
}

/// Runs the deletion with the log in `state`, a folder outside the scan.
fn clean_with(
    scan: &Scan,
    state: &Fixture,
    change: impl FnOnce(&mut CleanOptions),
) -> Vec<Removal> {
    let mut options = CleanOptions {
        trash: false,
        log_path: state.path("devdust/log.jsonl"),
        check_open_files: false,
        now: SystemTime::now(),
        only: None,
    };
    change(&mut options);
    execute(scan, &options, &|_| {}).unwrap()
}

fn clean(scan: &Scan, state: &Fixture) -> Vec<Removal> {
    clean_with(scan, state, |_| {})
}

/// The one removal a test expects, with its skip reason if it was skipped.
fn only(removals: &[Removal]) -> (&Path, Option<&str>) {
    assert_eq!(removals.len(), 1, "{removals:#?}");
    let reason = match &removals[0].outcome {
        Outcome::Skipped(reason) => Some(reason.as_str()),
        Outcome::Deleted => None,
        other => panic!("unexpected outcome {other:?}"),
    };
    (&removals[0].path, reason)
}

#[test]
fn cold_dust_is_deleted_and_logged() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "old");
    cold_repo(&fx, "new");
    fx.file("new/src/index.js", "// work");

    let removals = clean(&scan_fixture(&fx), &state);

    assert_eq!(
        only(&removals),
        (fx.path("old/node_modules").as_path(), None)
    );
    assert!(!fx.path("old/node_modules").exists());
    assert!(fx.path("old/package.json").exists());
    assert!(fx.path("old/src/index.js").exists());
    assert!(fx.path("new/node_modules/dep/index.js").exists());
    // Nothing is left behind under the temporary name.
    let left: Vec<_> = fs::read_dir(fx.path("old"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(".devdust"))
        .collect();
    assert!(left.is_empty(), "{left:?}");

    let log = fs::read_to_string(state.path("devdust/log.jsonl")).unwrap();
    let entry: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(entry["action"], "deleted");
    assert_eq!(entry["path"], fx.path("old/node_modules").to_str().unwrap());
    assert_eq!(entry["project"], fx.path("old").to_str().unwrap());
    assert_eq!(entry["rule"], "node");
    assert_eq!(entry["restore_hint"], "npm install");
    assert!(entry["size"].is_u64());
    assert!(entry["time"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn the_log_is_appended_to() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "a");
    clean(&scan_fixture(&fx), &state);
    cold_repo(&fx, "b");

    clean(&scan_fixture(&fx), &state);

    let log = fs::read_to_string(state.path("devdust/log.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2);
}

#[test]
fn only_the_chosen_projects_are_cleaned() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "a");
    cold_repo(&fx, "b");
    let scan = scan_fixture(&fx);
    let chosen = fx.path("b").canonicalize().unwrap();

    let removals = clean_with(&scan, &state, |options| {
        options.only = Some([chosen].into());
    });

    assert_eq!(removals.len(), 1, "{removals:#?}");
    assert!(fx.path("a/node_modules").exists());
    assert!(!fx.path("b/node_modules").exists());
}

#[test]
fn nothing_is_deleted_when_the_log_cannot_be_written() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "old");
    // A file where the log's folder should be.
    state.file("devdust", "");

    let options = CleanOptions {
        trash: false,
        log_path: state.path("devdust/log.jsonl"),
        check_open_files: false,
        now: SystemTime::now(),
        only: None,
    };
    let result = execute(&scan_fixture(&fx), &options, &|_| {});

    assert!(result.is_err());
    assert!(fx.path("old/node_modules").exists());
}

#[test]
fn a_project_that_became_active_is_left_alone() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "app");
    let scanned = scan_fixture(&fx);
    fx.file("app/src/index.js", "// back at work");

    let removals = clean(&scanned, &state);

    let (_, reason) = only(&removals);
    assert_eq!(reason, Some("the project became active: an edited file"));
    assert!(fx.path("app/node_modules").exists());
}

#[test]
fn dust_whose_manifest_is_gone_is_left_alone() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("app/Cargo.toml", "")
        .file("app/target/debug/app", "")
        .age("app", 100);
    let scanned = scan_fixture(&fx);
    fs::remove_file(fx.path("app/Cargo.toml")).unwrap();
    fx.age("app", 100);

    let removals = clean(&scanned, &state);

    assert_eq!(only(&removals).1, Some("its manifest is gone"));
    assert!(fx.path("app/target").exists());
}

#[test]
fn dust_that_git_now_tracks_is_left_alone() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "app");
    let scanned = scan_fixture(&fx);
    // Staged but not committed elsewhere would make the project warm, so
    // commit it, backdated, and keep every mtime old.
    fx.git("app", &["add", "-f", "node_modules"])
        .git_at("app", 100, &["commit", "-q", "-m", "vendor"])
        .age("app", 101);

    let removals = clean(&scanned, &state);

    assert_eq!(only(&removals).1, Some("contains files tracked by git"));
    assert!(fx.path("app/node_modules/dep/index.js").exists());
}

#[test]
fn dust_replaced_by_a_symlink_is_left_alone() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("app/Cargo.toml", "")
        .file("app/target/debug/app", "")
        .file("precious/data.txt", "")
        .age("app", 100);
    let scanned = scan_ignoring_age(&fx);
    fs::remove_dir_all(fx.path("app/target")).unwrap();
    symlink(fx.path("precious"), fx.path("app/target")).unwrap();

    let removals = clean(&scanned, &state);

    assert_eq!(only(&removals).1, Some("it is no longer a directory"));
    assert!(fx.path("precious/data.txt").exists());
}

#[test]
fn dust_reached_through_a_new_symlink_is_left_alone() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("app/Cargo.toml", "")
        .file("app/target/debug/app", "")
        .age("app", 100);
    let scanned = scan_ignoring_age(&fx);
    // The project folder is swapped for a link to a copy of itself.
    fs::rename(fx.path("app"), fx.path("moved")).unwrap();
    symlink(fx.path("moved"), fx.path("app")).unwrap();

    let removals = clean(&scanned, &state);

    assert_eq!(
        only(&removals).1,
        Some("its path now passes through a symlink")
    );
    assert!(fx.path("moved/target/debug/app").exists());
}

#[test]
fn dust_that_is_already_gone_is_reported() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("app/Cargo.toml", "")
        .file("app/target/debug/app", "")
        .age("app", 100);
    let scanned = scan_fixture(&fx);
    fs::remove_dir_all(fx.path("app/target")).unwrap();
    fx.age("app", 100);

    let removals = clean(&scanned, &state);

    assert_eq!(only(&removals).1, Some("it is already gone"));
}

#[cfg(target_os = "macos")]
#[test]
fn dust_with_open_files_is_left_alone() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("app/Cargo.toml", "")
        .file("app/target/debug/app", "")
        .age("app", 100);
    let scanned = scan_fixture(&fx);
    let _held_open = fs::File::open(fx.path("app/target/debug/app")).unwrap();

    let removals = clean_with(&scanned, &state, |o| o.check_open_files = true);

    assert_eq!(
        only(&removals).1,
        Some("a running process has files open in it")
    );
    assert!(fx.path("app/target").exists());
}

/// `code/repo` plus a linked worktree, both cold and both with dust.
fn repo_with_worktree(fx: &Fixture, worktree: &str) {
    cold_repo(fx, "code/repo");
    fx.git_at(
        "code/repo",
        100,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            &fx.path(worktree).to_string_lossy(),
        ],
    )
    .file(&format!("{worktree}/node_modules/dep/index.js"), "")
    .age(worktree, 101);
}

#[test]
fn locked_worktrees_need_include_locked() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    repo_with_worktree(&fx, "code/feature");
    fx.git("code/repo", &["worktree", "lock", "../feature"]);
    let mut options = ScanOptions::new(vec![fx.root().to_path_buf()]);

    clean(&scan(&options, &()).unwrap(), &state);
    assert!(!fx.path("code/repo/node_modules").exists());
    assert!(fx.path("code/feature/node_modules").exists());

    options.include.locked = true;
    clean(&scan(&options, &()).unwrap(), &state);
    assert!(!fx.path("code/feature/node_modules").exists());
}

#[test]
fn external_worktrees_need_include_external_worktrees() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    repo_with_worktree(&fx, "elsewhere/feature");
    let mut options = ScanOptions::new(vec![fx.path("code")]);

    clean(&scan(&options, &()).unwrap(), &state);
    assert!(!fx.path("code/repo/node_modules").exists());
    assert!(fx.path("elsewhere/feature/node_modules").exists());

    options.include.external_worktrees = true;
    clean(&scan(&options, &()).unwrap(), &state);
    assert!(!fx.path("elsewhere/feature/node_modules").exists());
    assert!(fx.path("elsewhere/feature/package.json").exists());
}

#[test]
fn an_interrupted_deletion_is_finished() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "app");
    // What a killed run leaves: the dust under its temporary name.
    let leftover = "app/.devdust-deleting-0123456789abcdef";
    fs::rename(fx.path("app/node_modules"), fx.path(leftover)).unwrap();
    fx.file("app/target/debug/x", "")
        .file("app/Cargo.toml", "")
        .git("app", &["add", "Cargo.toml"])
        .git_at("app", 100, &["commit", "-q", "-m", "more"])
        .file("app/.gitignore", "node_modules/\ntarget/\n")
        .git("app", &["add", ".gitignore"])
        .git_at("app", 100, &["commit", "-q", "-m", "ignore"])
        .age("app", 101);

    let scanned = scan_fixture(&fx);

    // The leftover is not dust, not a change, and does not hide real dust.
    assert_eq!(scanned.plan.interrupted, [fx.path(leftover)]);
    assert!(
        scanned.plan.projects[0].reclaimable(),
        "{:#?}",
        scanned.plan
    );
    let removals = clean(&scanned, &state);
    assert_eq!(removals.len(), 2);
    assert!(removals.iter().all(|r| r.outcome == Outcome::Deleted));
    assert!(!fx.path(leftover).exists());
    assert!(!fx.path("app/target").exists());
}

#[test]
fn the_log_can_be_read_back() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    cold_repo(&fx, "app");
    clean(&scan_fixture(&fx), &state);
    let log = state.path("devdust/log.jsonl");
    // A line cut short by a crash.
    let text = fs::read_to_string(&log).unwrap();
    fs::write(&log, format!("{text}{{\"time\":\"20")).unwrap();

    let records = read_log(&log).unwrap();

    assert_eq!(records.len(), 1, "{records:#?}");
    let app = fx.path("app").canonicalize().unwrap();
    assert_eq!(records[0].path, app.join("node_modules"));
    assert_eq!(records[0].project, app);
    assert_eq!(records[0].action, "deleted");
    assert_eq!(records[0].restore_hint, "npm install");
    assert!(read_log(&state.path("no-such-log")).unwrap().is_empty());
}
