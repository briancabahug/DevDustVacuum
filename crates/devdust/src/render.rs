//! Human-readable output.

use std::env;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use comfy_table::presets::NOTHING;
use comfy_table::{Cell, CellAlignment, Table};
use devdust_core::Plan;
use devdust_core::assess::Signal;
use devdust_core::config::{Config, default_roots};
use devdust_core::execute::{LogRecord, Outcome, Removal};
use devdust_core::plan::{Project, State};
use devdust_core::rules::RULES;

pub fn table(plan: &Plan) -> String {
    let home = env::var_os("HOME");
    let home = home.as_deref().map(Path::new);
    let totals = &plan.totals;
    let mut out = String::new();

    let (cold, kept): (Vec<&Project>, Vec<&Project>) =
        plan.projects.iter().partition(|p| p.reclaimable());
    if cold.is_empty() {
        out.push_str("No cold projects found.\n");
    } else {
        write_table(&mut out, "AGE", &cold, home, |project| {
            project.age_seconds.map_or("—".into(), format_age)
        });
        let reclaimable = &totals.reclaimable;
        let _ = write!(
            out,
            "\n{} · {} reclaimable",
            count(reclaimable.projects, "cold project"),
            format_size(reclaimable.apparent_bytes)
        );
        // Hardlinked files (pnpm, uv) only free space once every link is gone.
        if reclaimable.would_free_bytes != reclaimable.apparent_bytes {
            let _ = write!(
                out,
                " · deleting it would free {}",
                format_size(reclaimable.would_free_bytes)
            );
        }
        out.push('\n');
    }

    if !kept.is_empty() {
        let _ = writeln!(
            out,
            "\nKept ({}, {}):",
            count(totals.kept.projects, "project"),
            format_size(totals.kept.apparent_bytes)
        );
        write_table(&mut out, "WHY", &kept, home, why_kept);
    }

    if totals.below_min_size > 0 {
        let _ = writeln!(
            out,
            "\n{} below the minimum size not shown",
            count(totals.below_min_size, "project")
        );
    }
    if totals.unreadable > 0 {
        let _ = writeln!(
            out,
            "{} could not be read and were not counted",
            count(totals.unreadable as usize, "entry")
        );
    }
    if !plan.interrupted.is_empty() {
        let _ = writeln!(
            out,
            "\n{} left half deleted by an interrupted clean; the next clean --apply finishes the job",
            count(plan.interrupted.len(), "directory")
        );
    }
    if !plan.opted_out.is_empty() {
        let _ = writeln!(
            out,
            "\nNot searched, because they hold a .devdustkeep file:"
        );
        for folder in &plan.opted_out {
            let _ = writeln!(out, "  {}", display_path(folder, home));
        }
    }
    if !plan.skipped.is_empty() {
        let _ = writeln!(out, "\nSkipped:");
        for skipped in &plan.skipped {
            let _ = writeln!(
                out,
                "  {}: {}",
                display_path(&skipped.path, home),
                skipped.reason
            );
        }
    }
    out
}

fn write_table(
    out: &mut String,
    column: &str,
    projects: &[&Project],
    home: Option<&Path>,
    cell: impl Fn(&Project) -> String,
) {
    let mut table = Table::new();
    table
        .load_style(NOTHING)
        .set_header(["PROJECT", "BRANCH", column, "DUST", "SIZE"]);
    for project in projects {
        let branch = match (&project.worktree, &project.branch) {
            (None, _) => "—",
            (Some(_), None) => "detached",
            (Some(_), Some(branch)) => branch,
        };
        table.add_row([
            Cell::new(display_path(&project.path, home)),
            Cell::new(branch),
            Cell::new(cell(project)),
            Cell::new(dust_names(project)),
            Cell::new(format_size(project.size)).set_alignment(CellAlignment::Right),
        ]);
    }
    for line in table.to_string().lines() {
        let _ = writeln!(out, "{}", line.trim_end());
    }
}

/// One line per project for the interactive picker, in aligned columns.
pub fn choices(projects: &[&Project]) -> Vec<String> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let paths: Vec<String> = projects
        .iter()
        .map(|project| display_path(&project.path, home.as_deref()))
        .collect();
    let sizes: Vec<String> = projects.iter().map(|p| format_size(p.size)).collect();
    let path_width = paths.iter().map(|path| path.chars().count()).max();
    let size_width = sizes.iter().map(String::len).max();
    let (path_width, size_width) = (path_width.unwrap_or(0), size_width.unwrap_or(0));
    projects
        .iter()
        .zip(paths.iter().zip(&sizes))
        .map(|(project, (path, size))| {
            let age = project.age_seconds.map_or("—".into(), format_age);
            let dust = dust_names(project);
            format!("{path:<path_width$}  {size:>size_width$}  {age:>5}  {dust}")
        })
        .collect()
}

/// The removed directories that are still missing, newest first, grouped by
/// project, each with the command that brings it back.
pub fn restore_hints(missing: &[LogRecord]) -> String {
    if missing.is_empty() {
        return "Nothing to restore: no directory that devdust removed is still missing.\n".into();
    }
    let home = env::var_os("HOME").map(PathBuf::from);
    let mut projects: Vec<&Path> = Vec::new();
    for record in missing.iter().rev() {
        if !projects.contains(&record.project.as_path()) {
            projects.push(&record.project);
        }
    }
    let mut out = String::new();
    for project in projects {
        let _ = writeln!(out, "{}", display_path(project, home.as_deref()));
        let mut table = Table::new();
        table.load_style(NOTHING);
        for record in missing.iter().rev().filter(|r| r.project == project) {
            let directory = record.path.strip_prefix(project).unwrap_or(&record.path);
            // The date part of an RFC 3339 time.
            let day = record.time.get(..10).unwrap_or(&record.time);
            table.add_row([
                Cell::new(format!(" {}", directory.display())),
                Cell::new(&record.restore_hint),
                Cell::new(format!("{} {day}", record.action)),
                Cell::new(format_size(record.size)).set_alignment(CellAlignment::Right),
            ]);
        }
        for line in table.to_string().lines() {
            let _ = writeln!(out, "{}", line.trim_end());
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "Run each command in the folder its directory was in. A directory that was moved to the Trash can also be put back from there."
    );
    out
}

/// Why a project is not in the reclaimable list.
fn why_kept(project: &Project) -> String {
    let age = project.age_seconds.map_or("—".into(), format_age);
    match (project.state, &project.held) {
        (State::Unknown, held) => held.clone().unwrap_or_default(),
        (State::Cold, held) => format!("{}, cold {age}", held.as_deref().unwrap_or("held")),
        (State::Warm, _) => match project.signal {
            Some(Signal::Reflog) => format!("commit or checkout {age} ago"),
            Some(Signal::HeadCommit | Signal::PathCommit) => format!("committed {age} ago"),
            Some(Signal::SourceModified) => format!("files edited {age} ago"),
            Some(Signal::DustModified) => format!("dust rebuilt {age} ago"),
            Some(Signal::UncommittedChanges) => "uncommitted changes".into(),
            None => "warm".into(),
        },
    }
}

/// The kinds of dust in a project, largest first. A kind found more than
/// once, such as one `node_modules` per package, is shown as `name ×N`.
fn dust_names(project: &Project) -> String {
    let mut names: Vec<(&str, usize)> = Vec::new();
    for dust in &project.dust {
        match names.iter_mut().find(|(seen, _)| *seen == dust.name) {
            Some((_, times)) => *times += 1,
            None => names.push((dust.name, 1)),
        }
    }
    let names: Vec<String> = names
        .into_iter()
        .map(|(name, times)| match times {
            1 => name.to_string(),
            _ => format!("{name} ×{times}"),
        })
        .collect();
    names.join(", ")
}

fn format_age(seconds: u64) -> String {
    match seconds / 3600 {
        0 => "<1h".into(),
        hours @ 1..24 => format!("{hours}h"),
        hours => format!("{}d", hours / 24),
    }
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

pub fn count(n: usize, noun: &str) -> String {
    match (n, noun.strip_suffix('y')) {
        (1, _) => format!("1 {noun}"),
        (_, Some(stem)) => format!("{n} {stem}ies"),
        _ => format!("{n} {noun}s"),
    }
}

/// The config file's location and the settings in effect.
pub fn config(path: &Path, exists: bool, config: &Config) -> String {
    let home = env::var_os("HOME");
    let home = home.as_deref().map(Path::new);
    let paths = |paths: &[PathBuf]| {
        let shown: Vec<String> = paths.iter().map(|p| display_path(p, home)).collect();
        shown.join(", ")
    };
    let mut out = String::new();
    let note = if exists { "" } else { " (not created yet)" };
    let _ = writeln!(out, "Config file: {}{note}\n", display_path(path, home));

    let roots = match &config.roots {
        Some(roots) => paths(roots),
        None => format!("{} (default)", paths(&default_roots())),
    };
    let older_than = match config.older_than {
        // humantime would spell 42 days as "1month 11days 13h 30m 54s".
        Some(age) if age.as_secs() % 86_400 == 0 => format!("{}d", age.as_secs() / 86_400),
        Some(age) => humantime::format_duration(age).to_string(),
        None => "30d (default)".into(),
    };
    let min_size = config.min_size.map_or("none".into(), format_size);
    let custom: Vec<&str> = config
        .rules
        .iter()
        .filter_map(|rule| rule.id.strip_prefix("custom:"))
        .collect();
    let built_in = |wanted: bool| {
        let ids: Vec<&str> = RULES
            .iter()
            .filter(|rule| rule.opt_in == wanted)
            .filter(|rule| config.rules.iter().any(|on| on.id == rule.id) == wanted)
            .map(|rule| rule.id)
            .collect();
        ids.join(", ")
    };
    for (name, value) in [
        ("roots", roots),
        ("older_than", older_than),
        ("min_size", min_size),
        ("use_trash", config.use_trash.to_string()),
        ("ignore", config.ignore.join(", ")),
        ("rules enabled", built_in(true)),
        ("rules disabled", built_in(false)),
        ("custom rules", custom.join(", ")),
    ] {
        let value = if value.is_empty() { "none" } else { &value };
        let _ = writeln!(out, "{name:<15} {value}");
    }
    out
}

pub struct RemovalReport {
    pub text: String,
    /// Something was left in a state that needs attention.
    pub failed: bool,
}

/// What `clean --apply` did: totals first, then every directory that was not
/// removed and why.
pub fn removals(removals: &[Removal], log_path: &Path) -> RemovalReport {
    let home = env::var_os("HOME");
    let home = home.as_deref().map(Path::new);
    let mut text = String::new();

    for (outcome, verb) in [
        (Outcome::Deleted, "Deleted"),
        (Outcome::Trashed, "Moved to the Trash:"),
    ] {
        let done: Vec<&Removal> = removals.iter().filter(|r| r.outcome == outcome).collect();
        if !done.is_empty() {
            let _ = writeln!(
                text,
                "{verb} {} ({}).",
                count(done.len(), "directory"),
                format_size(done.iter().map(|r| r.size).sum())
            );
        }
    }

    let mut failed = false;
    for (title, pick) in [
        (
            "Left alone:",
            (|o| matches!(o, Outcome::Skipped(_))) as fn(&Outcome) -> bool,
        ),
        ("Failed:", |o| matches!(o, Outcome::Failed(_))),
    ] {
        let listed: Vec<&Removal> = removals.iter().filter(|r| pick(&r.outcome)).collect();
        if listed.is_empty() {
            continue;
        }
        let _ = writeln!(text, "\n{title}");
        for removal in listed {
            let (Outcome::Skipped(reason) | Outcome::Failed(reason)) = &removal.outcome else {
                continue;
            };
            failed |= matches!(removal.outcome, Outcome::Failed(_));
            let _ = writeln!(text, "  {}: {reason}", display_path(&removal.path, home));
        }
    }

    let _ = writeln!(
        text,
        "\nThe command that restores each directory is in {}",
        display_path(log_path, home)
    );
    RemovalReport { text, failed }
}

/// Decimal units, as Finder and most disk tools show them.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1_500), "1.5 kB");
        assert_eq!(format_size(612_000_000), "612 MB");
        assert_eq!(format_size(1_400_000_000), "1.4 GB");
        assert_eq!(format_size(5_000_000_000_000_000), "5000 TB");
    }

    #[test]
    fn counts() {
        assert_eq!(count(1, "directory"), "1 directory");
        assert_eq!(count(3, "directory"), "3 directories");
        assert_eq!(count(2, "entry"), "2 entries");
        assert_eq!(count(2, "cold project"), "2 cold projects");
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(59 * 60), "<1h");
        assert_eq!(format_age(23 * 3600), "23h");
        assert_eq!(format_age(24 * 3600), "1d");
        assert_eq!(format_age(214 * 86_400 + 5), "214d");
    }

    #[test]
    fn paths_under_home_are_shortened() {
        let home = Some(Path::new("/Users/me"));
        assert_eq!(
            display_path(Path::new("/Users/me/Code/app"), home),
            "~/Code/app"
        );
        assert_eq!(display_path(Path::new("/Users/me"), home), "~");
        assert_eq!(
            display_path(Path::new("/Users/meow/app"), home),
            "/Users/meow/app"
        );
        assert_eq!(display_path(Path::new("/srv/app"), None), "/srv/app");
    }
}
