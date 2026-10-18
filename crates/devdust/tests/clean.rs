use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use devdust_fixtures::Fixture;

/// Runs `devdust clean` on the fixture, with the action log kept in `state`
/// and `input` typed at the prompt.
fn clean(fx: &Fixture, state: &Fixture, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devdust"))
        .arg("clean")
        .args(args)
        .arg(fx.root())
        .env("XDG_STATE_HOME", state.root())
        .env("XDG_CONFIG_HOME", state.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// `shop`: a repo last touched 90 days ago. `tool`: edited just now.
fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.git_init("shop")
        .file("shop/package.json", "{}")
        .file("shop/.gitignore", "node_modules/\n")
        .sized_file("shop/node_modules/dep/blob.bin", 200_000)
        .commit_all_at("shop", 90)
        .age("shop", 91)
        .file("tool/Cargo.toml", "")
        .file("tool/target/debug/tool", "");
    fx
}

#[test]
fn without_apply_it_is_a_dry_run() {
    let (fx, state) = (fixture(), Fixture::new());

    let output = clean(&fx, &state, &[], "");

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("1 cold project · "));
    assert!(text.contains("Dry run: nothing was changed. Add --apply to delete this dust."));
    assert!(fx.path("shop/node_modules").exists());
    assert!(!state.path("devdust").exists());
}

#[test]
fn apply_asks_first() {
    let (fx, state) = (fixture(), Fixture::new());

    let refused = clean(&fx, &state, &["--apply"], "n\n");
    assert!(stdout(&refused).contains("Delete 1 directory ("));
    assert!(stdout(&refused).contains("Nothing was changed."));
    assert!(fx.path("shop/node_modules").exists());

    // No answer at all is a no.
    let silent = clean(&fx, &state, &["--apply"], "");
    assert!(stdout(&silent).contains("Nothing was changed."));
    assert!(fx.path("shop/node_modules").exists());

    let agreed = clean(&fx, &state, &["--apply"], "y\n");
    assert!(agreed.status.success());
    assert!(stdout(&agreed).contains("Deleted 1 directory ("));
    assert!(!fx.path("shop/node_modules").exists());
    assert!(fx.path("shop/package.json").exists());
    assert!(fx.path("tool/target").exists());
}

#[test]
fn yes_skips_the_question_and_the_log_says_how_to_restore() {
    let (fx, state) = (fixture(), Fixture::new());

    let output = clean(&fx, &state, &["--apply", "--yes"], "");

    assert!(output.status.success());
    assert!(!stdout(&output).contains("[y/N]"));
    assert!(!fx.path("shop/node_modules").exists());
    let log = fs::read_to_string(state.path("devdust/log.jsonl")).unwrap();
    let entry: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(
        entry["path"],
        fx.path("shop/node_modules").to_str().unwrap()
    );
    assert_eq!(entry["restore_hint"], "npm install");
    assert!(stdout(&output).contains("devdust/log.jsonl"));
}

#[test]
fn nothing_to_clean() {
    let (fx, state) = (Fixture::new(), Fixture::new());
    fx.file("tool/Cargo.toml", "")
        .file("tool/target/debug/tool", "");

    let output = clean(&fx, &state, &["--apply", "--yes"], "");

    assert!(output.status.success());
    assert!(stdout(&output).contains("Nothing to clean."));
    assert!(fx.path("tool/target").exists());
}

#[test]
fn interactive_refuses_to_run_without_a_terminal() {
    let (fx, state) = (fixture(), Fixture::new());

    let output = clean(&fx, &state, &["--apply", "--interactive"], "y\n");

    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(error.contains("--interactive needs a terminal"), "{error}");
    assert!(fx.path("shop/node_modules").exists());
}

#[test]
fn interactive_needs_apply() {
    let (fx, state) = (fixture(), Fixture::new());

    let output = clean(&fx, &state, &["--interactive"], "");

    assert!(!output.status.success());
    assert!(fx.path("shop/node_modules").exists());
}

fn restore_hints(state: &Fixture, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_devdust"))
        .arg("restore-hints")
        .args(args)
        .env("XDG_STATE_HOME", state.root())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    stdout(&output)
}

#[test]
fn restore_hints_lists_what_is_still_missing() {
    let (fx, state) = (fixture(), Fixture::new());
    let nothing = "Nothing to restore: no directory that devdust removed is still missing.\n";
    assert_eq!(restore_hints(&state, &[]), nothing);
    assert!(
        clean(&fx, &state, &["--apply", "--yes"], "")
            .status
            .success()
    );

    let text = restore_hints(&state, &[]);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].ends_with("/shop"), "{text}");
    assert!(
        lines[1].starts_with("  node_modules  npm install  deleted 20"),
        "{text}"
    );

    let shop = fx.path("shop");
    assert_eq!(restore_hints(&state, &[shop.to_str().unwrap()]), text);
    let tool = fx.path("tool");
    assert_eq!(restore_hints(&state, &[tool.to_str().unwrap()]), nothing);

    // Restored by hand: no longer missing.
    fx.dir("shop/node_modules");
    assert_eq!(restore_hints(&state, &[]), nothing);
}
