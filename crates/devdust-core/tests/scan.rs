use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use devdust_core::{Plan, ScanOptions, scan};
use devdust_fixtures::Fixture;

fn scan_fixture(fx: &Fixture) -> Plan {
    scan(&ScanOptions::new(vec![fx.root().to_path_buf()]), &())
        .unwrap()
        .plan
}

/// Every dust directory in the plan, relative to the fixture root, sorted.
fn dust_paths(fx: &Fixture, plan: &Plan) -> Vec<String> {
    let mut paths: Vec<String> = plan
        .projects
        .iter()
        .flat_map(|p| &p.dust)
        .map(|d| relative(fx, &d.path))
        .collect();
    paths.sort();
    paths
}

fn relative(fx: &Fixture, path: &Path) -> String {
    path.strip_prefix(fx.root())
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

/// The reason `rel` was skipped.
fn skip_reason<'a>(fx: &Fixture, plan: &'a Plan, rel: &str) -> &'a str {
    let path = fx.path(rel);
    plan.skipped
        .iter()
        .find(|s| s.path == path)
        .map(|s| s.reason.as_str())
        .unwrap_or_else(|| panic!("{rel} was not skipped: {:?}", plan.skipped))
}

#[test]
fn dust_needs_its_sibling_marker() {
    let fx = Fixture::new();
    fx.file("app/package.json", "{}")
        .file("app/node_modules/left-pad/index.js", "")
        .file("notes/node_modules/readme.txt", "")
        .file("crate/Cargo.toml", "")
        .file("crate/target/debug/app", "")
        .file("crate/src/target/keep.rs", "");

    let plan = scan_fixture(&fx);

    assert_eq!(dust_paths(&fx, &plan), ["app/node_modules", "crate/target"]);
    assert!(plan.skipped.is_empty());
}

#[test]
fn matched_dust_is_not_searched() {
    let fx = Fixture::new();
    fx.file("app/package.json", "{}")
        .file("app/node_modules/dep/package.json", "{}")
        .file("app/node_modules/dep/node_modules/nested/index.js", "");

    let plan = scan_fixture(&fx);

    assert_eq!(dust_paths(&fx, &plan), ["app/node_modules"]);
}

#[test]
fn directories_without_a_marker_are_still_searched() {
    let fx = Fixture::new();
    // `vendor` has no composer.json or go.mod next to it, so it is plain source.
    fx.file("site/vendor/widget/package.json", "{}")
        .file("site/vendor/widget/node_modules/dep/index.js", "");

    let plan = scan_fixture(&fx);

    assert_eq!(dust_paths(&fx, &plan), ["site/vendor/widget/node_modules"]);
}

#[test]
fn dependency_stores_without_a_manifest_are_left_alone() {
    let fx = Fixture::new();
    // Neither store has a manifest beside it, so neither is dust, and the
    // packages inside them are not projects.
    fx.file("lab/.venv/pyvenv.cfg", "")
        .file("lab/.venv/lib/site/pkg/__pycache__/a.pyc", "")
        .file("lab/.venv/lib/site/pkg/setup.py", "")
        .file("lab/.venv/lib/site/pkg/.venv/pyvenv.cfg", "")
        .file("notes/node_modules/dep/package.json", "{}")
        .file("notes/node_modules/dep/node_modules/nested/index.js", "");

    let plan = scan_fixture(&fx);

    assert!(dust_paths(&fx, &plan).is_empty());
    assert!(plan.skipped.is_empty());
}

#[test]
fn multi_part_rule_directories() {
    let fx = Fixture::new();
    fx.file("blog/Gemfile", "")
        .file("blog/vendor/bundle/ruby/gem.rb", "")
        .file("blog/vendor/assets/logo.svg", "")
        .file(
            "mobile/pubspec.yaml",
            "dependencies:\n  flutter:\n    sdk: flutter\n",
        )
        .file("mobile/ios/.symlinks/plugins/a", "")
        .file("mobile/ios/Flutter/ephemeral/x", "")
        .file("mobile/ios/Runner/main.swift", "")
        .file("cli/pubspec.yaml", "dependencies:\n  args: ^2.0.0\n")
        .file("cli/ios/.symlinks/plugins/a", "")
        .file("cli/.dart_tool/package_config.json", "");

    let plan = scan_fixture(&fx);

    assert_eq!(
        dust_paths(&fx, &plan),
        [
            "blog/vendor/bundle",
            "cli/.dart_tool",
            "mobile/ios/.symlinks",
            "mobile/ios/Flutter/ephemeral",
        ]
    );
    assert_eq!(
        skip_reason(&fx, &plan, "cli/ios/.symlinks"),
        "pubspec.yaml does not depend on Flutter"
    );
    let project = |rel: &str| {
        plan.projects
            .iter()
            .find(|p| p.path == fx.path(rel))
            .unwrap()
    };
    assert_eq!(project("mobile").dust.len(), 2);
    assert_eq!(project("cli").dust[0].restore_hint, "dart pub get");
}

#[test]
fn tracked_directories_are_skipped() {
    let fx = Fixture::new();
    fx.git_init("committed")
        .file("committed/go.mod", "module a\n")
        .file("committed/vendor/dep/dep.go", "package dep\n")
        .commit_all("committed")
        .git_init("fetched")
        .file("fetched/go.mod", "module b\n")
        .file("fetched/.gitignore", "vendor/\n")
        .file("fetched/vendor/dep/dep.go", "package dep\n")
        .commit_all("fetched");

    let plan = scan_fixture(&fx);

    assert_eq!(dust_paths(&fx, &plan), ["fetched/vendor"]);
    assert_eq!(
        skip_reason(&fx, &plan, "committed/vendor"),
        "contains files tracked by git"
    );
}

#[test]
fn build_output_must_be_git_ignored() {
    let fx = Fixture::new();
    fx.git_init("ignored")
        .file("ignored/package.json", "{}")
        .file("ignored/.gitignore", "dist/\n")
        .file("ignored/dist/app.js", "")
        .git_init("unignored")
        .file("unignored/package.json", "{}")
        .file("unignored/dist/app.js", "")
        .file("no-repo/package.json", "{}")
        .file("no-repo/dist/app.js", "");

    let plan = scan_fixture(&fx);

    assert_eq!(dust_paths(&fx, &plan), ["ignored/dist"]);
    assert_eq!(skip_reason(&fx, &plan, "unignored/dist"), "not git-ignored");
    assert_eq!(
        skip_reason(&fx, &plan, "no-repo/dist"),
        "not in a git repository, so it cannot be confirmed as git-ignored"
    );
}

#[test]
fn dust_in_a_submodule_is_checked_against_the_submodule() {
    let fx = Fixture::new();
    fx.git_init("lib")
        .file("lib/go.mod", "module lib\n")
        .file("lib/vendor/dep/dep.go", "package dep\n")
        .commit_all("lib")
        .git_init("app")
        .file("app/README.md", "")
        .commit_all("app");
    let lib = fx.path("lib");
    fx.git(
        "app",
        &["submodule", "--quiet", "add", lib.to_str().unwrap(), "lib"],
    );

    let plan = scan_fixture(&fx);

    // The parent repo sees `lib` as a single gitlink, with no files under it.
    assert_eq!(
        skip_reason(&fx, &plan, "app/lib/vendor"),
        "contains files tracked by git"
    );
    assert!(dust_paths(&fx, &plan).is_empty());
}

#[test]
fn symlinked_dust_is_ignored() {
    let fx = Fixture::new();
    fx.file("store/pkg/index.js", "")
        .file("app/package.json", "{}");
    symlink(fx.path("store"), fx.path("app/node_modules")).unwrap();

    let plan = scan_fixture(&fx);

    assert!(dust_paths(&fx, &plan).is_empty());
}

#[test]
fn marker_less_caches_join_the_nearest_project() {
    let fx = Fixture::new();
    fx.file("tool/pyproject.toml", "")
        .file("tool/src/tool/__pycache__/a.pyc", "")
        .file("tool/tests/__pycache__/b.pyc", "")
        .file("tool/.venv/pyvenv.cfg", "")
        .file("scripts/__pycache__/c.pyc", "")
        .file("scripts/lib/deep/__pycache__/d.pyc", "");

    let plan = scan_fixture(&fx);

    let tool = plan
        .projects
        .iter()
        .find(|p| p.path == fx.path("tool"))
        .unwrap();
    assert_eq!(tool.dust.len(), 3);
    // No manifest above them and no repo: the top-level folder is the project.
    let scripts = plan
        .projects
        .iter()
        .find(|p| p.path == fx.path("scripts"))
        .unwrap();
    assert_eq!(scripts.dust.len(), 2);
    assert_eq!(plan.projects.len(), 2);
}

#[test]
fn node_restore_hint_follows_the_lockfile() {
    let fx = Fixture::new();
    fx.git_init("mono")
        .file("mono/pnpm-lock.yaml", "")
        .file("mono/packages/web/package.json", "{}")
        .file("mono/packages/web/node_modules/dep/index.js", "")
        .file("plain/package.json", "{}")
        .file("plain/node_modules/dep/index.js", "");

    let plan = scan_fixture(&fx);

    let hint = |rel: &str| {
        let path = fx.path(rel);
        let project = plan.projects.iter().find(|p| p.path == path).unwrap();
        project.dust[0].restore_hint.as_str()
    };
    assert_eq!(hint("mono/packages/web"), "pnpm install");
    assert_eq!(hint("plain"), "npm install");
}

#[test]
fn sizes_match_du() {
    let fx = Fixture::new();
    fx.file("app/package.json", "{}")
        .sized_file("app/node_modules/big/blob.bin", 300_000)
        .sized_file("app/node_modules/small/one.js", 1)
        .sized_file("app/node_modules/small/deep/er/two.js", 5_000);

    let plan = scan_fixture(&fx);

    let output = Command::new("du")
        .arg("-sk")
        .arg(fx.path("app/node_modules"))
        .output()
        .unwrap();
    let du_kb: u64 = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let size = plan.projects[0].dust[0].size;
    assert!(size >= 305_001);
    assert_eq!(size.div_ceil(1024), du_kb);
    assert_eq!(plan.totals.kept.apparent_bytes, size);
    assert_eq!(plan.totals.kept.would_free_bytes, size);
}

#[test]
fn hardlinks_free_space_only_when_every_link_goes() {
    let fx = Fixture::new();
    fx.sized_file("store/shared.bin", 200_000)
        .file("a/package.json", "{}")
        .sized_file("a/node_modules/own.bin", 100_000)
        .file("b/package.json", "{}")
        .sized_file("b/node_modules/twin.bin", 400_000);
    // Linked from a store outside any dust directory, as pnpm does.
    fs::hard_link(
        fx.path("store/shared.bin"),
        fx.path("a/node_modules/shared.bin"),
    )
    .unwrap();
    // Both links inside dust, in two different projects.
    fs::hard_link(
        fx.path("b/node_modules/twin.bin"),
        fx.path("a/node_modules/twin.bin"),
    )
    .unwrap();

    let plan = scan_fixture(&fx);

    let size = |rel: &str| {
        let path = fx.path(rel);
        let project = plan.projects.iter().find(|p| p.path == path).unwrap();
        project.size
    };
    let (a, b) = (size("a"), size("b"));
    assert!(a >= 700_000, "a holds all three files: {a}");
    assert!((400_000..a).contains(&b), "b holds the twin only: {b}");
    // The twin is counted once across the two projects.
    let twin = b - fs::metadata(fx.path("b/node_modules")).map_or(0, |m| blocks(&m));
    assert_eq!(plan.totals.kept.apparent_bytes, a + b - twin);
    // The store keeps its copy of shared.bin, so that file frees nothing.
    let shared = blocks(&fs::metadata(fx.path("store/shared.bin")).unwrap());
    assert_eq!(
        plan.totals.kept.would_free_bytes,
        plan.totals.kept.apparent_bytes - shared
    );
}

fn blocks(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

#[test]
fn min_size_hides_small_projects() {
    let fx = Fixture::new();
    fx.file("big/package.json", "{}")
        .sized_file("big/node_modules/blob.bin", 200_000)
        .file("tiny/package.json", "{}")
        .sized_file("tiny/node_modules/blob.bin", 10);

    let mut options = ScanOptions::new(vec![fx.root().to_path_buf()]);
    options.min_size = 100_000;
    let plan = scan(&options, &()).unwrap().plan;

    assert_eq!(dust_paths(&fx, &plan), ["big/node_modules"]);
    assert_eq!(plan.totals.below_min_size, 1);
    assert_eq!(plan.totals.kept.apparent_bytes, plan.projects[0].size);
}

#[test]
fn overlapping_roots_are_scanned_once() {
    let fx = Fixture::new();
    fx.file("app/package.json", "{}")
        .file("app/node_modules/dep/index.js", "");

    let options = ScanOptions::new(vec![fx.root().to_path_buf(), fx.path("app")]);
    let plan = scan(&options, &()).unwrap().plan;

    assert_eq!(plan.totals.kept.dust_dirs, 1);
}

#[test]
fn missing_root_is_an_error() {
    let fx = Fixture::new();
    let options = ScanOptions::new(vec![fx.path("nope")]);
    assert!(scan(&options, &()).is_err());
}
