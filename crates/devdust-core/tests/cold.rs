use std::fs;
use std::time::Duration;

use devdust_core::assess::Signal;
use devdust_core::plan::{Project, State};
use devdust_core::{GroupBy, Plan, ScanOptions, scan};
use devdust_fixtures::Fixture;

fn scan_fixture(fx: &Fixture) -> Plan {
    scan_with(fx, |_| {})
}

fn scan_with(fx: &Fixture, change: impl FnOnce(&mut ScanOptions)) -> Plan {
    let mut options = ScanOptions::new(vec![fx.root().to_path_buf()]);
    change(&mut options);
    scan(&options, &()).unwrap().plan
}

fn project<'a>(fx: &Fixture, plan: &'a Plan, rel: &str) -> &'a Project {
    let path = fx.path(rel);
    plan.projects
        .iter()
        .find(|p| p.path == path)
        .unwrap_or_else(|| panic!("no project at {rel}: {:#?}", plan.projects))
}

fn age_days(project: &Project) -> u64 {
    project.age_seconds.unwrap() / 86_400
}

/// A Node project in its own repo, committed `days_ago` and untouched since.
/// Its files are a day older than the commit, so git holds the newest signal.
fn node_repo(fx: &Fixture, rel: &str, days_ago: u64) {
    fx.git_init(rel)
        .file(&format!("{rel}/package.json"), "{}")
        .file(&format!("{rel}/.gitignore"), "node_modules/\n")
        .file(&format!("{rel}/src/index.js"), "")
        .file(&format!("{rel}/node_modules/dep/index.js"), "")
        .commit_all_at(rel, days_ago)
        .age(rel, days_ago + 1);
}

#[test]
fn a_repo_is_cold_when_every_signal_is_old() {
    let fx = Fixture::new();
    node_repo(&fx, "old", 100);
    node_repo(&fx, "new", 3);

    let plan = scan_fixture(&fx);

    let old = project(&fx, &plan, "old");
    assert_eq!(old.state, State::Cold);
    assert!(old.reclaimable());
    assert_eq!(old.signal, Some(Signal::Reflog));
    assert_eq!(age_days(old), 100);
    assert_eq!(old.branch.as_deref(), Some("main"));
    let new = project(&fx, &plan, "new");
    assert_eq!(new.state, State::Warm);
    assert_eq!(new.signal, Some(Signal::Reflog));
    assert_eq!(plan.totals.reclaimable.projects, 1);
    assert_eq!(plan.totals.kept.projects, 1);
    assert_eq!(plan.projects[0].path, old.path);
}

#[test]
fn older_than_moves_the_threshold() {
    let fx = Fixture::new();
    node_repo(&fx, "app", 10);

    let cold_after = |days: u64| {
        let plan = scan_with(&fx, |o| o.older_than = Duration::from_secs(days * 86_400));
        project(&fx, &plan, "app").state == State::Cold
    };

    assert!(!cold_after(30));
    assert!(cold_after(7));
}

#[test]
fn each_signal_keeps_a_repo_warm() {
    let fx = Fixture::new();
    for name in ["checkout", "edit", "install", "dirty"] {
        node_repo(&fx, name, 100);
    }
    // Checking out an old commit is activity, though the commit date is old.
    fx.git("checkout", &["checkout", "-q", "-b", "review"])
        .age("checkout", 100);
    fx.file("edit/notes.txt", "");
    fx.dir("install/node_modules/new-dep");
    // A tracked file with an uncommitted change and an old mtime.
    fx.file("dirty/src/index.js", "changed").age("dirty", 100);

    let plan = scan_fixture(&fx);

    let signal = |rel: &str| {
        let project = project(&fx, &plan, rel);
        assert_eq!(project.state, State::Warm, "{rel}");
        project.signal.unwrap()
    };
    assert_eq!(signal("checkout"), Signal::Reflog);
    assert_eq!(signal("edit"), Signal::SourceModified);
    assert_eq!(signal("install"), Signal::DustModified);
    assert_eq!(signal("dirty"), Signal::UncommittedChanges);
}

#[test]
fn head_commit_date_stands_in_for_a_missing_reflog() {
    let fx = Fixture::new();
    node_repo(&fx, "app", 100);
    fs::remove_file(fx.path("app/.git/logs/HEAD")).unwrap();

    let plan = scan_fixture(&fx);

    let app = project(&fx, &plan, "app");
    assert_eq!(app.state, State::Cold);
    assert_eq!(app.signal, Some(Signal::HeadCommit));
    assert_eq!(age_days(app), 100);
}

#[test]
fn a_repo_without_commits_is_judged_by_its_files() {
    let fx = Fixture::new();
    fx.git_init("app")
        .file("app/package.json", "{}")
        .file("app/.gitignore", "*\n")
        .file("app/node_modules/dep/index.js", "")
        .age("app", 100);

    let plan = scan_fixture(&fx);

    assert_eq!(project(&fx, &plan, "app").state, State::Cold);
}

#[test]
fn untracked_dust_does_not_keep_a_repo_warm() {
    let fx = Fixture::new();
    fx.git_init("app")
        .file("app/package.json", "{}")
        .commit_all_at("app", 100)
        // Not git-ignored, so git status reports it as untracked.
        .file("app/node_modules/dep/index.js", "")
        .age("app", 100);

    let plan = scan_fixture(&fx);

    assert_eq!(project(&fx, &plan, "app").state, State::Cold);
}

#[test]
fn projects_outside_git_are_judged_by_mtime() {
    let fx = Fixture::new();
    for name in ["old", "new"] {
        fx.file(&format!("{name}/Cargo.toml"), "")
            .file(&format!("{name}/src/main.rs"), "")
            .file(&format!("{name}/target/debug/app"), "")
            .age(name, 60);
    }
    fx.file("new/src/main.rs", "fn main() {}");
    // Finder's bookkeeping is not activity.
    fx.file("old/.DS_Store", "");

    let plan = scan_fixture(&fx);

    let old = project(&fx, &plan, "old");
    assert_eq!(old.state, State::Cold);
    assert_eq!(age_days(old), 60);
    assert_eq!(old.branch, None);
    let new = project(&fx, &plan, "new");
    assert_eq!(new.state, State::Warm);
    assert_eq!(new.signal, Some(Signal::SourceModified));
}

/// A repo of unrelated projects: `a` untouched for 100 days, `b` committed
/// to 2 days ago. Every mtime is older, so only git can tell them apart.
fn grab_bag(fx: &Fixture) {
    fx.git_init("lab")
        .file("lab/.gitignore", "node_modules/\ntarget/\n")
        .file("lab/a/package.json", "{}")
        .file("lab/a/node_modules/dep/index.js", "")
        .file("lab/b/Cargo.toml", "")
        .file("lab/b/target/debug/b", "")
        .commit_all_at("lab", 100)
        .file("lab/b/src/main.rs", "")
        .commit_all_at("lab", 2)
        .age("lab", 101);
}

#[test]
fn unrelated_projects_in_one_repo_are_judged_separately() {
    let fx = Fixture::new();
    grab_bag(&fx);

    let plan = scan_fixture(&fx);

    // The repo's reflog is 2 days old, but it covers the whole worktree.
    let a = project(&fx, &plan, "lab/a");
    assert_eq!(a.state, State::Cold);
    assert_eq!(a.signal, Some(Signal::PathCommit));
    assert_eq!(age_days(a), 100);
    let b = project(&fx, &plan, "lab/b");
    assert_eq!(b.state, State::Warm);
    assert_eq!(b.signal, Some(Signal::PathCommit));
}

#[test]
fn uncommitted_changes_only_warm_their_own_project() {
    let fx = Fixture::new();
    grab_bag(&fx);
    fx.git_at("lab", 100, &["commit", "-q", "--amend", "--no-edit"])
        .file("lab/b/src/main.rs", "fn main() {}")
        .age("lab", 100);

    let plan = scan_fixture(&fx);

    assert_eq!(project(&fx, &plan, "lab/a").state, State::Cold);
    let b = project(&fx, &plan, "lab/b");
    assert_eq!(b.signal, Some(Signal::UncommittedChanges));
}

#[test]
fn group_by_overrides_the_grouping() {
    let fx = Fixture::new();
    grab_bag(&fx);

    let by_repo = scan_with(&fx, |o| o.group_by = Some(GroupBy::Repo));

    assert_eq!(by_repo.projects.len(), 1);
    let lab = project(&fx, &by_repo, "lab");
    assert_eq!(lab.dust.len(), 2);
    assert_eq!(lab.state, State::Warm);
    assert_eq!(lab.signal, Some(Signal::Reflog));
}

#[test]
fn a_declared_workspace_is_one_unit() {
    let fx = Fixture::new();
    fx.git_init("mono")
        .file("mono/.gitignore", "node_modules/\n")
        .file("mono/tools/gen/package.json", "{}")
        .file("mono/tools/gen/node_modules/dep/index.js", "")
        // The workspace is declared in a subfolder, not at the repo root.
        .file(
            "mono/frontend/package.json",
            r#"{"workspaces": ["packages/*"]}"#,
        )
        .file("mono/frontend/node_modules/dep/index.js", "")
        .file("mono/frontend/packages/web/package.json", "{}")
        .file("mono/frontend/packages/web/node_modules/dep/index.js", "")
        .file("mono/frontend/packages/admin/package.json", "{}")
        .file("mono/frontend/packages/admin/node_modules/dep/index.js", "")
        .commit_all_at("mono", 100)
        .file("mono/frontend/packages/web/index.js", "")
        .commit_all_at("mono", 1)
        .age("mono", 100);

    let plan = scan_fixture(&fx);

    // Work in one package keeps the whole workspace warm, admin included.
    let frontend = project(&fx, &plan, "mono/frontend");
    assert_eq!(frontend.dust.len(), 3);
    assert_eq!(frontend.state, State::Warm);
    assert_eq!(project(&fx, &plan, "mono/tools/gen").state, State::Cold);
    assert_eq!(plan.projects.len(), 2);

    let by_project = scan_with(&fx, |o| o.group_by = Some(GroupBy::Project));
    let admin = project(&fx, &by_project, "mono/frontend/packages/admin");
    assert_eq!(admin.state, State::Cold);
}

#[test]
fn other_workspace_declarations() {
    let fx = Fixture::new();
    fx.file("rust/Cargo.toml", "[workspace]\nmembers = [\"a\"]\n")
        .file("rust/target/debug/x", "")
        .file("rust/a/Cargo.toml", "[package]\n")
        .file("rust/a/target/debug/x", "")
        .file("pnpm/pnpm-workspace.yaml", "")
        .file("pnpm/apps/web/package.json", "{}")
        .file("pnpm/apps/web/node_modules/dep/index.js", "");

    let plan = scan_fixture(&fx);

    assert_eq!(project(&fx, &plan, "rust").dust.len(), 2);
    assert_eq!(project(&fx, &plan, "pnpm").dust.len(), 1);
}

#[test]
fn a_project_nested_in_another_is_judged_with_it() {
    let fx = Fixture::new();
    fx.file(
        "app/pubspec.yaml",
        "dependencies:\n  flutter:\n    sdk: flutter\n",
    )
    .file("app/.dart_tool/package_config.json", "")
    .file("app/ios/Podfile", "")
    .file("app/ios/Pods/Manifest.lock", "")
    .age("app", 100)
    // Only Dart code was edited; ios/ itself is untouched.
    .file("app/lib/main.dart", "");

    let plan = scan_fixture(&fx);

    let app = project(&fx, &plan, "app");
    assert_eq!(app.dust.len(), 2);
    assert_eq!(app.state, State::Warm);
    assert_eq!(plan.projects.len(), 1);
}

/// `repo` with dust, last touched 100 days ago, plus a linked worktree at
/// `worktree` on branch `feature` that also has dust.
fn repo_with_worktree(fx: &Fixture, worktree: &str) {
    node_repo(fx, "code/repo", 100);
    fx.dir("code")
        .git_at(
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
fn each_worktree_is_judged_on_its_own() {
    let fx = Fixture::new();
    repo_with_worktree(&fx, "code/repo-feature");
    fx.file("code/repo/src/index.js", "// work")
        .commit_all("code/repo");

    let plan = scan_fixture(&fx);

    let main = project(&fx, &plan, "code/repo");
    assert_eq!(main.state, State::Warm);
    assert_eq!(main.branch.as_deref(), Some("main"));
    // The commit in the main worktree is not activity in the linked one.
    let feature = project(&fx, &plan, "code/repo-feature");
    assert_eq!(feature.state, State::Cold);
    assert_eq!(feature.signal, Some(Signal::Reflog));
    assert_eq!(age_days(feature), 100);
    assert_eq!(feature.branch.as_deref(), Some("feature"));
    assert_eq!(feature.worktree.as_deref(), Some(feature.path.as_path()));
}

#[test]
fn a_nested_worktree_is_not_activity_for_its_parent() {
    let fx = Fixture::new();
    node_repo(&fx, "repo", 100);
    fx.file("repo/.git/info/exclude", ".worktrees/\n")
        .git_at(
            "repo",
            100,
            &["worktree", "add", "-q", "--detach", ".worktrees/spike"],
        )
        .file("repo/.worktrees/spike/node_modules/dep/index.js", "")
        .age("repo", 100)
        .file("repo/.worktrees/spike/src/index.js", "// work");

    let plan = scan_fixture(&fx);

    assert_eq!(project(&fx, &plan, "repo").state, State::Cold);
    let spike = project(&fx, &plan, "repo/.worktrees/spike");
    assert_eq!(spike.state, State::Warm);
    assert_eq!(spike.branch, None);
}

#[test]
fn a_locked_worktree_is_held() {
    let fx = Fixture::new();
    repo_with_worktree(&fx, "code/repo-feature");
    fx.git("code/repo", &["worktree", "lock", "../repo-feature"]);

    let plan = scan_fixture(&fx);

    let feature = project(&fx, &plan, "code/repo-feature");
    assert_eq!(feature.state, State::Cold);
    assert_eq!(feature.held.as_deref(), Some("locked worktree"));
    assert!(!feature.reclaimable());
    assert!(project(&fx, &plan, "code/repo").reclaimable());
    assert_eq!(plan.totals.reclaimable.projects, 1);
}

#[test]
fn a_worktree_outside_the_roots_is_reported_but_held() {
    let fx = Fixture::new();
    repo_with_worktree(&fx, "elsewhere/feature");

    let options = ScanOptions::new(vec![fx.path("code")]);
    let plan = scan(&options, &()).unwrap().plan;

    let feature = project(&fx, &plan, "elsewhere/feature");
    assert_eq!(feature.state, State::Cold);
    assert_eq!(feature.held.as_deref(), Some("worktree outside the roots"));
    assert!(project(&fx, &plan, "code/repo").reclaimable());
}

#[test]
fn a_bare_repo_with_only_linked_worktrees() {
    let fx = Fixture::new();
    node_repo(&fx, "seed", 100);
    fx.git(".", &["clone", "-q", "--bare", "seed", "repo.git"])
        .git_at(
            "repo.git",
            100,
            &["worktree", "add", "-q", "../wt-old", "main"],
        )
        .git_at(
            "repo.git",
            100,
            &["worktree", "add", "-q", "-b", "next", "../wt-new"],
        )
        .file("wt-old/node_modules/dep/index.js", "")
        .file("wt-new/node_modules/dep/index.js", "")
        .age("wt-old", 100)
        .age("wt-new", 100)
        .file("wt-new/src/index.js", "// work");

    let plan = scan_fixture(&fx);

    let old = project(&fx, &plan, "wt-old");
    assert_eq!(old.state, State::Cold);
    assert_eq!(old.branch.as_deref(), Some("main"));
    assert_eq!(project(&fx, &plan, "wt-new").state, State::Warm);
}

#[test]
fn dust_in_a_submodule_belongs_to_the_containing_worktree() {
    let fx = Fixture::new();
    fx.git_init("lib")
        .file("lib/package.json", "{}")
        .file("lib/.gitignore", "node_modules/\n")
        .commit_all_at("lib", 100)
        .git_init("app")
        .file("app/README.md", "")
        .commit_all_at("app", 100);
    let lib = fx.path("lib");
    fx.git_at(
        "app",
        100,
        &["submodule", "--quiet", "add", lib.to_str().unwrap(), "lib"],
    )
    .commit_all_at("app", 100)
    .file("app/lib/node_modules/dep/index.js", "")
    .age("app", 100);

    let plan = scan_fixture(&fx);

    let lib = project(&fx, &plan, "app/lib");
    assert_eq!(lib.worktree, Some(fx.path("app")));
    assert_eq!(lib.branch.as_deref(), Some("main"));
    assert_eq!(lib.state, State::Cold, "{lib:#?}");
}
