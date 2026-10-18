//! Command-line definitions.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "devdust", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List the dependency and build directories of cold projects. Read-only.
    Scan(ScanArgs),
    /// Delete the dust of cold projects. A dry run unless --apply is given.
    Clean(CleanArgs),
    /// Show how to bring back the directories that were removed and are
    /// still missing.
    RestoreHints(RestoreHintsArgs),
    /// Show where the config file is and what it sets.
    Config(ConfigArgs),
    /// Set up a weekly scan that notifies you about dust. It never deletes.
    Schedule(ScheduleArgs),
    /// Print a completion script for a shell.
    Completions(CompletionsArgs),
}

/// What decides which projects are found and which of them are cold.
#[derive(Args)]
pub struct SelectArgs {
    /// Folders to scan [default: the config's roots, else ~/Code ~/Projects
    /// ~/src ~/dev ~/Developer]
    pub roots: Vec<PathBuf>,

    /// How long a project must be untouched to count as cold [default: the
    /// config's older_than, else 30d]
    #[arg(long, value_name = "AGE", value_parser = humantime::parse_duration)]
    pub older_than: Option<Duration>,

    /// Judge whole repos, or every project on its own, instead of the
    /// default: a declared workspace, else the outermost project
    #[arg(long, value_enum, value_name = "UNIT")]
    pub group_by: Option<GroupBy>,

    /// Leave out projects with less dust than this, for example 50MB
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub min_size: Option<u64>,

    /// Also scan other filesystems and volumes mounted below the roots
    #[arg(long)]
    pub cross_devices: bool,
}

#[derive(Args)]
pub struct ScanArgs {
    #[command(flatten)]
    pub select: SelectArgs,

    /// Print JSON instead of a table
    #[arg(long)]
    pub json: bool,

    /// Also show a desktop notification when there is dust to reclaim
    #[arg(long)]
    pub notify: bool,
}

#[derive(Args)]
pub struct CleanArgs {
    #[command(flatten)]
    pub select: SelectArgs,

    /// Really delete. Without this, clean only shows what it would do
    #[arg(long)]
    pub apply: bool,

    /// Move the dust to the Trash instead of deleting it [default: the
    /// config's use_trash]
    #[arg(long)]
    pub trash: bool,

    /// Do not ask for confirmation
    #[arg(long, short)]
    pub yes: bool,

    /// Pick which cold projects to clean from a list
    #[arg(long, short, requires = "apply")]
    pub interactive: bool,

    /// Also clean linked worktrees that lie outside the folders being scanned
    #[arg(long)]
    pub include_external_worktrees: bool,

    /// Also clean worktrees locked with `git worktree lock`
    #[arg(long)]
    pub include_locked: bool,
}

#[derive(Args)]
pub struct RestoreHintsArgs {
    /// Only show what was removed inside this folder
    pub project: Option<PathBuf>,
}

#[derive(Args)]
pub struct ConfigArgs {
    /// Open the config file in $VISUAL or $EDITOR, creating it if needed
    #[arg(long)]
    pub edit: bool,
}

#[derive(Args)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub action: ScheduleAction,
}

#[derive(Subcommand)]
pub enum ScheduleAction {
    /// Install the weekly scan, replacing an earlier one
    Install {
        /// Print the files instead of installing them
        #[arg(long)]
        print: bool,
    },
    /// Remove the weekly scan
    Remove,
    /// Show whether the weekly scan is installed
    Status,
}

#[derive(Args)]
pub struct CompletionsArgs {
    pub shell: clap_complete::Shell,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum GroupBy {
    Repo,
    Project,
}

fn parse_size(text: &str) -> Result<u64, String> {
    text.parse::<bytesize::ByteSize>().map(|size| size.as_u64())
}
