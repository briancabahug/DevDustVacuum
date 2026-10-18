use std::process::{Command, Output};

use devdust_fixtures::Fixture;

/// Runs devdust with `config` as the config file. The fixture root stands
/// in for the home folder, so `~` in the config points into it.
fn devdust(fx: &Fixture, config: &str, args: &[&str]) -> Output {
    fx.file("xdg/devdust/config.toml", config);
    Command::new(env!("CARGO_BIN_EXE_devdust"))
        .args(args)
        .env("HOME", fx.root())
        .env("XDG_CONFIG_HOME", fx.path("xdg"))
        .env("XDG_STATE_HOME", fx.path("state"))
        .env_remove("EDITOR")
        .env_remove("VISUAL")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Two cold projects outside git: `work/big` (200 kB of dust, 60 days old)
/// and `work/small` (10 days old).
fn fixture() -> Fixture {
    let fx = Fixture::new();
    fx.file("work/big/Cargo.toml", "")
        .sized_file("work/big/target/debug/big", 200_000)
        .age("work/big", 60)
        .file("work/small/Cargo.toml", "")
        .file("work/small/target/debug/small", "")
        .age("work/small", 10);
    fx
}

#[test]
fn the_config_supplies_what_the_command_line_leaves_out() {
    let fx = fixture();
    let config = "roots = [\"~/work\"]\nolder_than = \"7d\"\nmin_size = \"100kB\"";

    let from_config = stdout(&devdust(&fx, config, &["scan"]));
    assert!(from_config.contains("~/work/big"));
    assert!(from_config.contains("1 cold project · "));
    assert!(from_config.contains("1 project below the minimum size not shown"));

    let overridden = stdout(&devdust(
        &fx,
        config,
        &["scan", "--min-size", "0", "--older-than", "30d"],
    ));
    assert!(overridden.contains("1 cold project · "));
    let kept = overridden.split("Kept (1 project, ").nth(1).unwrap();
    assert!(kept.contains("~/work/small"));

    let other_root = fx.path("work/small");
    let narrowed = stdout(&devdust(
        &fx,
        config,
        &["scan", "--min-size", "0", other_root.to_str().unwrap()],
    ));
    assert!(narrowed.contains("~/work/small") && !narrowed.contains("~/work/big"));
}

#[test]
fn use_trash_changes_what_clean_would_do() {
    let fx = fixture();

    let output = devdust(&fx, "roots = [\"~/work\"]\nuse_trash = true", &["clean"]);

    // A dry run: this test must never really move anything to the Trash.
    let text = stdout(&output);
    assert!(text.contains("Add --apply to move to the Trash this dust."));
    assert!(fx.path("work/big/target").exists());
}

#[test]
fn an_invalid_config_stops_everything() {
    let fx = fixture();

    let output = devdust(
        &fx,
        "roots = [\"~/work\"]\nolder_then = \"7d\"",
        &["clean", "--apply", "--yes"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid config") && stderr.contains("older_then"));
    assert!(fx.path("work/big/target").exists());
}

#[test]
fn config_shows_the_settings_in_effect() {
    let fx = fixture();
    let config = r#"
        roots = ["~/work"]
        older_than = "6w"
        ignore = ["**/fixtures/**"]
        [rules]
        disable = ["dotnet"]
        [[rules.custom]]
        dir = ".cache-build"
        marker = "Makefile"
        restore = "make"
    "#;

    let text = stdout(&devdust(&fx, config, &["config"]));

    assert!(text.contains("Config file: ~/xdg/devdust/config.toml\n"));
    let value = |name: &str| {
        let line = text.lines().find(|line| line.starts_with(name)).unwrap();
        line[name.len()..].trim().to_string()
    };
    assert_eq!(value("roots"), "~/work");
    assert_eq!(value("older_than"), "42d");
    assert_eq!(value("min_size"), "none");
    assert_eq!(value("use_trash"), "false");
    assert_eq!(value("ignore"), "**/fixtures/**");
    assert_eq!(value("rules enabled"), "none");
    assert_eq!(value("rules disabled"), "dotnet");
    assert_eq!(value("custom rules"), ".cache-build");
}

#[test]
fn config_edit_needs_an_editor() {
    let fx = Fixture::new();

    let output = devdust(&fx, "", &["config", "--edit"]);

    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("$EDITOR")
    );
}

#[test]
fn completions_prints_a_script_for_the_shell() {
    let output = Command::new(env!("CARGO_BIN_EXE_devdust"))
        .args(["completions", "zsh"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let script = String::from_utf8(output.stdout).unwrap();
    assert!(script.starts_with("#compdef devdust"), "{script}");
    assert!(script.contains("--older-than"));
}

#[test]
fn schedule_install_print_shows_the_job_and_installs_nothing() {
    let home = Fixture::new();

    let output = Command::new(env!("CARGO_BIN_EXE_devdust"))
        .args(["schedule", "install", "--print"])
        .env("HOME", home.root())
        .env("XDG_CONFIG_HOME", home.path("config"))
        .env("XDG_STATE_HOME", home.path("state"))
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(env!("CARGO_BIN_EXE_devdust")), "{text}");
    assert!(text.contains("--notify"));
    assert!(text.contains(home.path("config").to_str().unwrap()));
    assert_eq!(std::fs::read_dir(home.root()).unwrap().count(), 0);
}

#[test]
fn schedule_status_and_remove_without_a_job() {
    let home = Fixture::new();
    for action in ["status", "remove"] {
        let output = Command::new(env!("CARGO_BIN_EXE_devdust"))
            .args(["schedule", action])
            .env("HOME", home.root())
            .output()
            .unwrap();

        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text, "The weekly scan is not installed.\n");
    }
}
