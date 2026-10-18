use std::process::{Command, Output};

use devdust_fixtures::Fixture;
use serde_json::Value;

fn devdust(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devdust"))
        .args(args)
        // No config file there, so the user's own config cannot leak in.
        .env("XDG_CONFIG_HOME", "/var/empty")
        .output()
        .unwrap()
}

/// `shop`: a repo last touched 90 days ago. `tool`: a project outside git,
/// edited just now.
fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.git_init("shop")
        .file("shop/package.json", "{}")
        .file("shop/package-lock.json", "{}")
        .file("shop/.gitignore", "node_modules/\n")
        .sized_file("shop/node_modules/dep/blob.bin", 200_000)
        .sized_file("shop/dist/app.js", 50_000)
        .commit_all_at("shop", 90)
        .age("shop", 91)
        .file("tool/Cargo.toml", "")
        .sized_file("tool/target/debug/tool", 10);
    fx
}

#[test]
fn table_separates_cold_from_kept() {
    let fx = fixture();

    let output = devdust(&["scan", fx.root().to_str().unwrap()]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].contains("PROJECT") && lines[0].contains("AGE"));
    for part in ["shop", "main", "90d", "node_modules"] {
        assert!(lines[1].contains(part), "{part} missing from {}", lines[1]);
    }
    assert!(stdout.contains("1 cold project · "));
    let kept = stdout.split("Kept (1 project, ").nth(1).unwrap();
    let tool = kept.lines().find(|line| line.contains("tool")).unwrap();
    assert!(tool.contains("ago") && tool.contains("target"));
    assert!(stdout.contains("shop/dist: contains files tracked by git"));
}

#[test]
fn older_than_takes_units() {
    let fx = fixture();

    let output = devdust(&["scan", "--older-than", "26w", fx.root().to_str().unwrap()]);

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("No cold projects found."));
    assert!(stdout.contains("Kept (2 projects, "));
    assert!(stdout.contains("commit or checkout 90d ago"));
}

#[test]
fn json_output() {
    let fx = fixture();

    let output = devdust(&["scan", "--json", fx.root().to_str().unwrap()]);

    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let shop = &plan["projects"][0];
    assert_eq!(shop["path"], fx.path("shop").to_str().unwrap());
    assert_eq!(shop["worktree"], fx.path("shop").to_str().unwrap());
    assert_eq!(shop["branch"], "main");
    assert_eq!(shop["state"], "cold");
    assert_eq!(shop["signal"], "reflog");
    assert_eq!(shop["age_seconds"].as_u64().unwrap() / 86_400, 90);
    assert_eq!(shop["held"], Value::Null);
    let dust = &shop["dust"][0];
    assert_eq!(dust["path"], fx.path("shop/node_modules").to_str().unwrap());
    assert_eq!(dust["name"], "node_modules");
    assert_eq!(dust["rule"], "node");
    assert_eq!(dust["ecosystem"], "Node.js");
    assert_eq!(dust["restore_hint"], "npm ci");
    assert!(dust["size"].as_u64().unwrap() >= 200_000);
    assert_eq!(plan["projects"][1]["state"], "warm");
    assert_eq!(plan["projects"][1]["worktree"], Value::Null);
    assert_eq!(plan["totals"]["reclaimable"]["projects"], 1);
    assert_eq!(plan["totals"]["kept"]["dust_dirs"], 1);
    assert_eq!(
        plan["skipped"][0]["reason"],
        "contains files tracked by git"
    );
}

#[test]
fn group_by_repo() {
    let fx = fixture();
    fx.file("shop/tools/Cargo.toml", "")
        .file("shop/tools/target/debug/x", "");

    let output = devdust(&[
        "scan",
        "--json",
        "--group-by",
        "repo",
        fx.root().to_str().unwrap(),
    ]);

    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let shop = plan["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == fx.path("shop").to_str().unwrap())
        .unwrap();
    assert_eq!(shop["dust"].as_array().unwrap().len(), 2);
}

#[test]
fn min_size_takes_units() {
    let fx = fixture();

    let output = devdust(&["scan", "--min-size", "100kB", fx.root().to_str().unwrap()]);

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("1 cold project · "));
    assert!(!stdout.contains("Kept"));
    assert!(stdout.contains("1 project below the minimum size not shown"));
}

#[test]
fn missing_root_fails() {
    let fx = Fixture::new();

    let output = devdust(&["scan", fx.path("nope").to_str().unwrap()]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cannot scan")
    );
}

/// Runs `scan --notify` with stand-ins for the notification programs first
/// on `PATH`. They write their arguments to `sent` in the returned fixture.
fn scan_notify(fx: &Fixture) -> (Output, Fixture) {
    use std::os::unix::fs::PermissionsExt;
    let bin = Fixture::new();
    for name in ["osascript", "notify-send"] {
        let script = format!(
            "#!/bin/sh\necho \"$@\" > '{}'\n",
            bin.path("sent").display()
        );
        bin.file(name, &script);
        let mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(bin.path(name), mode).unwrap();
    }
    let path = format!("{}:{}", bin.root().display(), env!("PATH"));
    let output = Command::new(env!("CARGO_BIN_EXE_devdust"))
        .args(["scan", "--notify", fx.root().to_str().unwrap()])
        .env("XDG_CONFIG_HOME", "/var/empty")
        .env("PATH", path)
        .output()
        .unwrap();
    (output, bin)
}

#[test]
fn notify_reports_the_cold_dust() {
    let (output, bin) = scan_notify(&fixture());

    assert!(output.status.success(), "{output:?}");
    let sent = std::fs::read_to_string(bin.path("sent")).unwrap();
    assert!(sent.contains("devdust"), "{sent}");
    assert!(
        sent.contains("of dev dust in 1 cold project. Run `devdust clean`"),
        "{sent}"
    );
}

#[test]
fn notify_stays_quiet_without_cold_dust() {
    let fx = Fixture::new();
    fx.file("tool/Cargo.toml", "")
        .sized_file("tool/target/debug/tool", 10);

    let (output, bin) = scan_notify(&fx);

    assert!(output.status.success(), "{output:?}");
    assert!(!bin.path("sent").exists());
}
