mod cli;
mod render;

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use devdust_core::config::{self, Config};
use devdust_core::execute::{self, CleanOptions, LogRecord};
use devdust_core::plan::Project;
use devdust_core::schedule::{Job, Manager, notify};
use devdust_core::{Include, Progress, Scan, ScanOptions};
use dialoguer::MultiSelect;
use indicatif::ProgressBar;

use cli::{CleanArgs, Cli, Command, ConfigArgs, GroupBy, ScanArgs, ScheduleAction, SelectArgs};

fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Scan(args) => scan(args),
        Command::Clean(args) => clean(args),
        Command::RestoreHints(args) => restore_hints(args.project),
        Command::Config(args) => show_config(args),
        Command::Schedule(args) => schedule(args.action),
        Command::Completions(args) => {
            let mut command = Cli::command();
            clap_complete::generate(args.shell, &mut command, "devdust", &mut io::stdout());
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn scan(args: ScanArgs) -> Result<ExitCode> {
    let plan = run_scan(args.select, load_config()?, Include::default())?.plan;
    let mut out = io::stdout().lock();
    if args.json {
        serde_json::to_writer_pretty(&mut out, &plan)?;
        writeln!(out)?;
    } else {
        write!(out, "{}", render::table(&plan))?;
    }
    let reclaimable = &plan.totals.reclaimable;
    if args.notify && reclaimable.projects > 0 {
        let text = format!(
            "{} of dev dust in {}. Run `devdust clean` to review it.",
            render::format_size(reclaimable.apparent_bytes),
            render::count(reclaimable.projects, "cold project")
        );
        notify("devdust", &text)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn clean(args: CleanArgs) -> Result<ExitCode> {
    let include = Include {
        locked: args.include_locked,
        external_worktrees: args.include_external_worktrees,
    };
    let config = load_config()?;
    let trash = args.trash || config.use_trash;
    let scan = run_scan(args.select, config, include)?;
    let plan = &scan.plan;
    print!("{}", render::table(plan));

    let reclaimable = &plan.totals.reclaimable;
    let (mut directories, mut bytes) = (reclaimable.dust_dirs, reclaimable.apparent_bytes);
    directories += plan.interrupted.len();
    if directories == 0 {
        println!("\nNothing to clean.");
        return Ok(ExitCode::SUCCESS);
    }
    let action = if trash { "move to the Trash" } else { "delete" };
    if !args.apply {
        println!("\nDry run: nothing was changed. Add --apply to {action} this dust.");
        return Ok(ExitCode::SUCCESS);
    }
    let mut only = None;
    if args.interactive && reclaimable.projects > 0 {
        let cold: Vec<&Project> = plan.projects.iter().filter(|p| p.reclaimable()).collect();
        let picked = pick(&cold, action)?;
        if picked.is_empty() {
            println!("Nothing was picked, so nothing was changed.");
            return Ok(ExitCode::SUCCESS);
        }
        let picked = picked.into_iter().map(|index| cold[index]);
        directories = plan.interrupted.len() + picked.clone().map(|p| p.dust.len()).sum::<usize>();
        bytes = picked.clone().map(|p| p.size).sum();
        only = Some(picked.map(|p| p.path.clone()).collect::<HashSet<_>>());
    }
    let question = format!(
        "\n{} {} ({})? [y/N] ",
        capitalize(action),
        render::count(directories, "directory"),
        render::format_size(bytes)
    );
    if !args.yes && !confirm(&question)? {
        println!("Nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }

    let log_path = config::log_path().context("cannot place the action log: HOME is not set")?;
    let options = CleanOptions {
        trash,
        log_path,
        check_open_files: true,
        now: SystemTime::now(),
        only,
    };
    let bar = ProgressBar::new(directories as u64);
    let removals = execute::execute(&scan, &options, &|_| bar.inc(1));
    bar.finish_and_clear();
    let removals = removals?;

    let report = render::removals(&removals, &options.log_path);
    print!("\n{}", report.text);
    Ok(if report.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn restore_hints(project: Option<PathBuf>) -> Result<ExitCode> {
    let log_path = config::log_path().context("cannot find the action log: HOME is not set")?;
    // The log holds resolved paths. A project folder that is gone cannot be
    // resolved, so it is compared as written.
    let inside = match project {
        Some(folder) => Some(match folder.canonicalize() {
            Ok(resolved) => resolved,
            Err(_) => std::path::absolute(&folder)?,
        }),
        None => None,
    };
    let mut missing: Vec<LogRecord> = Vec::new();
    for record in execute::read_log(&log_path)? {
        // A directory removed more than once is listed once, as last removed.
        missing.retain(|earlier| earlier.path != record.path);
        missing.push(record);
    }
    missing.retain(|record| {
        let wanted = inside.as_ref();
        wanted.is_none_or(|folder| record.path.starts_with(folder))
            && record.path.symlink_metadata().is_err()
    });
    print!("{}", render::restore_hints(&missing));
    Ok(ExitCode::SUCCESS)
}

fn schedule(action: ScheduleAction) -> Result<ExitCode> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let manager = Manager::current();
    match action {
        ScheduleAction::Install { print } => {
            // The job gets today's PATH, so it finds the same git, and the
            // same config and state folders.
            let env = ["PATH", "XDG_CONFIG_HOME", "XDG_STATE_HOME"]
                .iter()
                .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?)))
                .collect();
            let log = config::log_path().context("HOME is not set")?;
            let job = Job {
                program: std::env::current_exe().context("cannot find the devdust program")?,
                env,
                log: log.with_file_name("schedule.log"),
            };
            if print {
                for (path, contents) in manager.files(&job, &home) {
                    println!("# {}\n{contents}", path.display());
                }
                return Ok(ExitCode::SUCCESS);
            }
            manager.install(&job, &home)?;
            println!(
                "Installed: `{} scan --notify` runs {}.",
                job.program.display(),
                manager.when()
            );
            println!("It notifies you when there is dust to reclaim, and never deletes.");
            if manager == Manager::Launchd {
                println!("Its output goes to {}", job.log.display());
            }
        }
        ScheduleAction::Remove => match manager.remove(&home)? {
            true => println!("Removed the weekly scan."),
            false => println!("The weekly scan is not installed."),
        },
        ScheduleAction::Status => {
            let status = manager.status(&home);
            match (status.files.is_empty(), status.loaded) {
                (true, _) => println!("The weekly scan is not installed."),
                (false, true) => println!("The weekly scan runs {}.", manager.when()),
                (false, false) => println!(
                    "The weekly scan is installed but not loaded. Run `devdust schedule install` again."
                ),
            }
            for file in status.files {
                println!("  {}", file.display());
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn config_path() -> Result<PathBuf> {
    config::config_path().context("cannot place the config file: HOME is not set")
}

fn load_config() -> Result<Config> {
    Ok(Config::load(&config_path()?)?)
}

/// Runs a scan. Each setting comes from the command line, else from the
/// config file, else from the built-in default.
fn run_scan(args: SelectArgs, config: Config, include: Include) -> Result<Scan> {
    let roots = if !args.roots.is_empty() {
        args.roots
    } else if let Some(roots) = config.roots {
        roots
    } else {
        let roots = config::default_roots();
        if roots.is_empty() {
            bail!(
                "none of the default folders exist (~/Code, ~/Projects, ~/src, ~/dev, ~/Developer); pass the folders to scan, or set roots in the config file"
            );
        }
        roots
    };
    let mut options = ScanOptions::new(roots);
    options.min_size = args.min_size.or(config.min_size).unwrap_or(0);
    options.cross_devices = args.cross_devices;
    if let Some(older_than) = args.older_than.or(config.older_than) {
        options.older_than = older_than;
    }
    options.include = include;
    options.rules = config.rules;
    options.ignore = config.ignore_set;
    options.group_by = args.group_by.map(|unit| match unit {
        GroupBy::Repo => devdust_core::GroupBy::Repo,
        GroupBy::Project => devdust_core::GroupBy::Project,
    });

    let spinner = Spinner::start();
    let scan = devdust_core::scan(&options, &spinner);
    spinner.bar.finish_and_clear();
    Ok(scan?)
}

const CONFIG_TEMPLATE: &str = r#"# devdust configuration. Every setting is optional.

# roots = ["~/Code", "~/Work"]
# older_than = "30d"
# min_size = "20MB"
# use_trash = false

# Paths that are never searched.
# ignore = [
#   "~/Code/client-x/**",
#   "**/fixtures/**",
# ]

# [rules]
# enable = ["unignored-build"]
# disable = ["dotnet"]

# [[rules.custom]]
# dir = ".cache-build"
# marker = "Makefile"
# restore = "make"
"#;

fn show_config(args: ConfigArgs) -> Result<ExitCode> {
    let path = config_path()?;
    if args.edit {
        let editor = ["VISUAL", "EDITOR"]
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
            .with_context(|| format!("set $EDITOR, or edit {} yourself", path.display()))?;
        if !path.exists() {
            if let Some(folder) = path.parent() {
                fs::create_dir_all(folder)?;
            }
            fs::write(&path, CONFIG_TEMPLATE)?;
        }
        // $EDITOR may carry arguments, as in "code --wait".
        let mut words = editor.split_whitespace();
        let program = words.next().context("$EDITOR is empty")?;
        let status = std::process::Command::new(program)
            .args(words)
            .arg(&path)
            .status()
            .with_context(|| format!("cannot run {editor}"))?;
        if !status.success() {
            bail!("{editor} exited with {status}");
        }
    }

    let exists = path.exists();
    let config = Config::load(&path)?;
    print!("{}", render::config(&path, exists, &config));
    Ok(ExitCode::SUCCESS)
}

/// Shows the cold projects as a checklist with nothing ticked, and returns
/// the positions of the ones the user ticks.
fn pick(cold: &[&Project], action: &str) -> Result<Vec<usize>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("--interactive needs a terminal");
    }
    let picked = MultiSelect::new()
        .with_prompt(format!(
            "Pick the projects to {action} (space ticks, a ticks all, enter confirms, esc cancels)"
        ))
        .items(render::choices(cold))
        .report(false)
        .interact_opt()?;
    Ok(picked.unwrap_or_default())
}

/// Asks on the terminal. Anything but `y` or `yes`, including no input at
/// all, is a no.
fn confirm(question: &str) -> Result<bool> {
    print!("{question}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn capitalize(text: &str) -> String {
    let mut letters = text.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => String::new(),
    }
}

/// Progress on stderr. indicatif hides it when stderr is not a terminal.
struct Spinner {
    bar: ProgressBar,
    bytes: AtomicU64,
}

impl Spinner {
    fn start() -> Self {
        let bar = ProgressBar::new_spinner().with_message("Looking for dust…");
        bar.enable_steady_tick(Duration::from_millis(100));
        Self {
            bar,
            bytes: AtomicU64::new(0),
        }
    }
}

impl Progress for Spinner {
    fn discovered(&self, dust_dirs: usize) {
        self.bar.set_length(dust_dirs as u64);
    }

    fn measured(&self, bytes: u64) {
        let total = self.bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.bar.inc(1);
        self.bar.set_message(format!(
            "Measured {} of {} directories, {} so far",
            self.bar.position(),
            self.bar.length().unwrap_or(0),
            render::format_size(total)
        ));
    }

    fn assessing(&self) {
        self.bar.set_message("Checking for recent activity…");
    }
}
