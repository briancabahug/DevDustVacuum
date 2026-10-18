use std::path::Path;
use std::time::{Duration, SystemTime};

use devdust_core::config::Config;
use devdust_core::execute::{CleanOptions, Outcome, execute};
use devdust_core::{Plan, ScanOptions, scan};
use devdust_fixtures::Fixture;

fn parse(text: &str) -> Config {
    Config::parse(text, Some(Path::new("/Users/me"))).unwrap()
}

fn error(text: &str) -> String {
    Config::parse(text, Some(Path::new("/Users/me"))).unwrap_err()
}

fn scan_with(fx: &Fixture, config: &str) -> Plan {
    let config = parse(config);
    let mut options = ScanOptions::new(vec![fx.root().to_path_buf()]);
    options.rules = config.rules;
    options.ignore = config.ignore_set;
    scan(&options, &()).unwrap().plan
}

/// Every dust directory in the plan, relative to the fixture root, sorted.
fn dust_paths(fx: &Fixture, plan: &Plan) -> Vec<String> {
    let mut paths: Vec<String> = plan
        .projects
        .iter()
        .flat_map(|p| &p.dust)
        .map(|d| {
            d.path
                .strip_prefix(fx.root())
                .unwrap()
                .display()
                .to_string()
        })
        .collect();
    paths.sort();
    paths
}

fn skip_reason<'a>(fx: &Fixture, plan: &'a Plan, rel: &str) -> &'a str {
    let path = fx.path(rel);
    let skipped = plan.skipped.iter().find(|s| s.path == path);
    &skipped
        .unwrap_or_else(|| panic!("{rel} was not skipped"))
        .reason
}

#[test]
fn every_setting() {
    let config = parse(
        r#"
        roots = ["~/Code", "/srv/work", "~"]
        older_than = "6w"
        min_size = "20MB"
        use_trash = true
        ignore = ["~/Code/client-x/**", "**/fixtures/**"]

        [rules]
        enable = ["unignored-build"]
        disable = ["dotnet"]

        [[rules.custom]]
        dir = ".cache-build"
        marker = "Makefile"
        restore = "make"
        "#,
    );

    let roots = config.roots.unwrap();
    let roots: Vec<&str> = roots.iter().map(|root| root.to_str().unwrap()).collect();
    assert_eq!(roots, ["/Users/me/Code", "/srv/work", "/Users/me"]);
    assert_eq!(config.older_than, Some(Duration::from_secs(42 * 86_400)));
    assert_eq!(config.min_size, Some(20_000_000));
    assert!(config.use_trash);
    assert!(config.ignore_set.is_match("/Users/me/Code/client-x/app"));
    assert!(!config.ignore_set.is_match("/Users/me/Code/client-y/app"));
    let ids: Vec<&str> = config.rules.iter().map(|rule| rule.id).collect();
    // Custom rules come first, so they win over a built-in of the same name.
    assert_eq!(ids[0], "custom:.cache-build");
    assert!(ids.contains(&"unignored-build"));
    assert!(!ids.contains(&"dotnet"));
    assert!(ids.contains(&"node"));
}

#[test]
fn an_empty_file_changes_nothing() {
    let config = parse("");

    assert_eq!(config.roots, None);
    assert_eq!(config.older_than, None);
    assert_eq!(config.min_size, None);
    assert!(!config.use_trash);
    let ids: Vec<&str> = config.rules.iter().map(|rule| rule.id).collect();
    assert!(ids.contains(&"dotnet") && !ids.contains(&"unignored-build"));
}

#[test]
fn mistakes_are_errors() {
    assert!(error("older_then = \"30d\"").contains("older_then"));
    assert!(error("[rules]\nenabled = []").contains("enabled"));
    assert!(error("older_than = \"soon\"").contains("older_than = \"soon\""));
    assert!(error("min_size = \"big\"").contains("min_size = \"big\""));
    assert!(error("ignore = [\"a/[\"]").contains("ignore pattern"));
    assert!(error("[rules]\ndisable = [\"nodejs\"]").contains("unknown rule \"nodejs\""));
    let custom = |dir: &str, marker: &str| {
        error(&format!(
            "[[rules.custom]]\ndir = \"{dir}\"\nmarker = \"{marker}\"\nrestore = \"make\""
        ))
    };
    assert!(custom("/tmp/cache", "Makefile").contains("relative path"));
    assert!(custom("../cache", "Makefile").contains("relative path"));
    assert!(custom(".git", "Makefile").contains("relative path"));
    assert!(custom("cache", "").contains("needs a marker"));
    assert!(error("[[rules.custom]]\ndir = \"cache\"").contains("marker"));
}

#[test]
fn a_devdustkeep_file_stops_the_search() {
    let fx = Fixture::new();
    fx.file("kept/.devdustkeep", "")
        .file("kept/package.json", "{}")
        .file("kept/node_modules/dep/index.js", "")
        .file("kept/tools/Cargo.toml", "")
        .file("kept/tools/target/debug/x", "")
        .file("other/package.json", "{}")
        .file("other/node_modules/dep/index.js", "");

    let plan = scan_with(&fx, "");

    assert_eq!(dust_paths(&fx, &plan), ["other/node_modules"]);
    assert_eq!(plan.opted_out, [fx.path("kept")]);
}

#[test]
fn a_manifest_can_opt_its_project_out() {
    let fx = Fixture::new();
    fx.file("node/package.json", r#"{"devdust": {"keep": true}}"#)
        .file("node/node_modules/dep/index.js", "")
        .file(
            "php/composer.json",
            r#"{"extra": {"devdust": {"keep": true}}}"#,
        )
        .file("php/vendor/autoload.php", "")
        .file(
            "rust/Cargo.toml",
            "[package.metadata.devdust]\nkeep = true\n",
        )
        .file("rust/target/debug/x", "")
        .file("python/pyproject.toml", "[tool.devdust]\nkeep = true\n")
        .file("python/.venv/pyvenv.cfg", "")
        .file("python/src/__pycache__/a.pyc", "")
        // A workspace root speaks for its packages.
        .file(
            "mono/package.json",
            r#"{"workspaces": ["*"], "devdust": {"keep": true}}"#,
        )
        .file("mono/web/package.json", "{}")
        .file("mono/web/node_modules/dep/index.js", "")
        // Saying no, or nothing, keeps nothing.
        .file("plain/package.json", r#"{"devdust": {"keep": false}}"#)
        .file("plain/node_modules/dep/index.js", "");

    let plan = scan_with(&fx, "");

    assert_eq!(dust_paths(&fx, &plan), ["plain/node_modules"]);
    let reason = skip_reason(&fx, &plan, "node/node_modules");
    let manifest = fx.path("node/package.json");
    assert_eq!(
        reason,
        format!("kept: {} sets devdust.keep", manifest.display())
    );
    for rel in [
        "php/vendor",
        "rust/target",
        "python/.venv",
        "python/src/__pycache__",
        "mono/web/node_modules",
    ] {
        assert!(skip_reason(&fx, &plan, rel).starts_with("kept: "), "{rel}");
    }
}

#[test]
fn a_keep_added_after_the_scan_is_honoured() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("app/package.json", "{}")
        .file("app/node_modules/dep/index.js", "")
        .age("app", 100);
    let mut options = ScanOptions::new(vec![fx.root().to_path_buf()]);
    options.older_than = Duration::ZERO;
    let scanned = scan(&options, &()).unwrap();
    fx.file("app/package.json", r#"{"devdust": {"keep": true}}"#);

    let options = CleanOptions {
        trash: false,
        log_path: state.path("log.jsonl"),
        check_open_files: false,
        now: SystemTime::now(),
        only: None,
    };
    let removals = execute(&scanned, &options, &|_| {}).unwrap();

    assert!(
        matches!(&removals[0].outcome, Outcome::Skipped(reason) if reason.starts_with("kept: "))
    );
    assert!(fx.path("app/node_modules").exists());
}

#[test]
fn ignored_paths_are_not_searched() {
    let fx = Fixture::new();
    for project in [
        "client-x/app",
        "mine/app",
        "mine/tests/fixtures/sample",
        "mine/scratch",
    ] {
        fx.file(&format!("{project}/package.json"), "{}")
            .file(&format!("{project}/node_modules/dep/index.js"), "");
    }
    let config = format!(
        "ignore = [\"{}/client-x/**\", \"**/fixtures/**\", \"scratch\"]",
        fx.root().display()
    );

    let plan = scan_with(&fx, &config);

    assert_eq!(dust_paths(&fx, &plan), ["mine/app/node_modules"]);
    assert!(plan.skipped.is_empty());
}

#[test]
fn rules_can_be_disabled_and_enabled() {
    let fx = Fixture::new();
    fx.git_init("web")
        .file("web/package.json", "{}")
        .file("web/node_modules/dep/index.js", "")
        // Not git-ignored: dust only for the opt-in rule.
        .file("web/dist/app.js", "")
        .file("web/build/app.js", "")
        .file("web/coverage/index.html", "");

    let default = scan_with(&fx, "");
    assert_eq!(dust_paths(&fx, &default), ["web/node_modules"]);
    assert_eq!(skip_reason(&fx, &default, "web/dist"), "not git-ignored");

    let changed = scan_with(
        &fx,
        "[rules]\nenable = [\"unignored-build\"]\ndisable = [\"node\"]",
    );
    assert_eq!(dust_paths(&fx, &changed), ["web/build", "web/dist"]);
    // Tracked files are still off limits for the opt-in rule.
    fx.commit_all("web");
    let committed = scan_with(&fx, "[rules]\nenable = [\"unignored-build\"]");
    assert_eq!(
        skip_reason(&fx, &committed, "web/dist"),
        "contains files tracked by git"
    );
}

#[test]
fn custom_rules() {
    let fx = Fixture::new();
    fx.file("firmware/Makefile", "")
        .file("firmware/.cache-build/obj/a.o", "")
        .file("firmware/out/gen/image.bin", "")
        .file("firmware/gen/source.c", "")
        .file("notes/.cache-build/stuff", "");
    let config = r#"
        [[rules.custom]]
        dir = ".cache-build"
        marker = "Makefile"
        restore = "make"

        [[rules.custom]]
        dir = "out/gen"
        marker = "Makefile"
        restore = "make image"
    "#;

    let plan = scan_with(&fx, config);

    // No Makefile beside it in notes/, and gen/ is only dust below out/.
    assert_eq!(
        dust_paths(&fx, &plan),
        ["firmware/.cache-build", "firmware/out/gen"]
    );
    let firmware = &plan.projects[0];
    assert_eq!(firmware.path, fx.path("firmware"));
    let cache = firmware
        .dust
        .iter()
        .find(|d| d.name == ".cache-build")
        .unwrap();
    assert_eq!(cache.rule, "custom:.cache-build");
    assert_eq!(cache.ecosystem, "Custom");
    assert_eq!(cache.restore_hint, "make");
}
