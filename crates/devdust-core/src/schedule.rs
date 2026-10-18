//! The weekly scan: a launchd agent on macOS, a systemd user timer elsewhere.
//! The job only scans and notifies; it never deletes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Error;

const LAUNCHD_LABEL: &str = "com.devdust.scan";
const SYSTEMD_UNIT: &str = "devdust-scan";

/// What the job runs: `program scan --notify`.
pub struct Job {
    pub program: PathBuf,
    /// Environment variables the job gets, such as the `PATH` git is on.
    pub env: Vec<(String, String)>,
    /// Where launchd sends the job's output. systemd keeps it in the journal.
    pub log: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Launchd,
    Systemd,
}

#[derive(Debug)]
pub struct Status {
    /// The job's files that exist.
    pub files: Vec<PathBuf>,
    /// Whether the service manager has the job loaded.
    pub loaded: bool,
}

impl Manager {
    /// The service manager of the system this was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Launchd
        } else {
            Self::Systemd
        }
    }

    /// Every Monday at 10:00. A run missed while the machine was asleep
    /// happens when it wakes.
    pub fn when(self) -> &'static str {
        "every Monday at 10:00"
    }

    fn paths(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Self::Launchd => vec![
                home.join("Library/LaunchAgents")
                    .join(format!("{LAUNCHD_LABEL}.plist")),
            ],
            Self::Systemd => {
                let units = home.join(".config/systemd/user");
                vec![
                    units.join(format!("{SYSTEMD_UNIT}.service")),
                    units.join(format!("{SYSTEMD_UNIT}.timer")),
                ]
            }
        }
    }

    /// The files that make up the job, with their contents.
    pub fn files(self, job: &Job, home: &Path) -> Vec<(PathBuf, String)> {
        let contents = match self {
            Self::Launchd => vec![launchd_plist(job)],
            Self::Systemd => vec![systemd_service(job), systemd_timer()],
        };
        self.paths(home).into_iter().zip(contents).collect()
    }

    /// Writes the job's files and loads the job, replacing an earlier one.
    pub fn install(self, job: &Job, home: &Path) -> Result<(), Error> {
        if self == Self::Launchd {
            // launchd does not create the folder of the output file.
            if let Some(folder) = job.log.parent() {
                fs::create_dir_all(folder).map_err(|source| file_error(folder, source))?;
            }
            // Loading a job that is already loaded fails.
            let _ = run("launchctl", &["bootout", &launchd_target()?]);
        }
        let files = self.files(job, home);
        for (path, contents) in &files {
            if let Some(folder) = path.parent() {
                fs::create_dir_all(folder).map_err(|source| file_error(folder, source))?;
            }
            fs::write(path, contents).map_err(|source| file_error(path, source))?;
        }
        match self {
            Self::Launchd => {
                let plist = files[0].0.to_string_lossy();
                run("launchctl", &["bootstrap", &launchd_domain()?, &plist])
            }
            Self::Systemd => {
                run("systemctl", &["--user", "daemon-reload"])?;
                let timer = format!("{SYSTEMD_UNIT}.timer");
                run("systemctl", &["--user", "enable", "--now", &timer])
            }
        }
    }

    /// Unloads the job and deletes its files. Returns whether there was one.
    pub fn remove(self, home: &Path) -> Result<bool, Error> {
        let paths: Vec<PathBuf> = self
            .paths(home)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if paths.is_empty() {
            return Ok(false);
        }
        // The job may not be loaded; the files are deleted either way.
        let _ = match self {
            Self::Launchd => run("launchctl", &["bootout", &launchd_target()?]),
            Self::Systemd => {
                let timer = format!("{SYSTEMD_UNIT}.timer");
                run("systemctl", &["--user", "disable", "--now", &timer])
            }
        };
        for path in &paths {
            fs::remove_file(path).map_err(|source| file_error(path, source))?;
        }
        if self == Self::Systemd {
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
        Ok(true)
    }

    pub fn status(self, home: &Path) -> Status {
        let files: Vec<PathBuf> = self
            .paths(home)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        let loaded = !files.is_empty()
            && match self {
                Self::Launchd => launchd_target()
                    .and_then(|target| run("launchctl", &["print", &target]))
                    .is_ok(),
                Self::Systemd => {
                    let timer = format!("{SYSTEMD_UNIT}.timer");
                    run("systemctl", &["--user", "is-active", "--quiet", &timer]).is_ok()
                }
            };
        Status { files, loaded }
    }
}

/// Shows a desktop notification.
pub fn notify(title: &str, text: &str) -> Result<(), Error> {
    if cfg!(target_os = "macos") {
        // The text is passed as an argument, so nothing in it is read as
        // AppleScript.
        let script = [
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            "--",
        ];
        run("osascript", &[script.as_slice(), &[title, text]].concat())
    } else {
        run("notify-send", &["--", title, text])
    }
}

fn launchd_plist(job: &Job) -> String {
    let mut env = String::new();
    for (name, value) in &job.env {
        env += &format!(
            "        <key>{}</key>\n        <string>{}</string>\n",
            xml(name),
            xml(value)
        );
    }
    let (program, log) = (
        xml(&job.program.to_string_lossy()),
        xml(&job.log.to_string_lossy()),
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>scan</string>
        <string>--notify</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
{env}    </dict>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Weekday</key>
        <integer>1</integer>
        <key>Hour</key>
        <integer>10</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn systemd_service(job: &Job) -> String {
    let mut env = String::new();
    for (name, value) in &job.env {
        env += &format!(
            "Environment={}\n",
            systemd_quote(&format!("{name}={value}"))
        );
    }
    format!(
        "[Unit]\nDescription=devdust weekly scan\n\n[Service]\nType=oneshot\n{env}ExecStart={} scan --notify\n",
        // `$` is only special in the command line, not in Environment=.
        systemd_quote(&job.program.to_string_lossy()).replace('$', "$$")
    )
}

fn systemd_timer() -> String {
    "[Unit]\nDescription=devdust weekly scan\n\n[Timer]\nOnCalendar=Mon 10:00\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n".into()
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Quotes a value for a unit file, where `%` starts a specifier.
fn systemd_quote(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// launchd's name for the logged-in user's jobs.
fn launchd_domain() -> Result<String, Error> {
    let output = Command::new("id")
        .arg("-u")
        .stderr(Stdio::null())
        .output()
        .map_err(|error| Error::Schedule(format!("could not run id: {error}")))?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || uid.is_empty() {
        return Err(Error::Schedule("could not find the user id".into()));
    }
    Ok(format!("gui/{uid}"))
}

fn launchd_target() -> Result<String, Error> {
    Ok(format!("{}/{LAUNCHD_LABEL}", launchd_domain()?))
}

fn run(program: &str, args: &[&str]) -> Result<(), Error> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(|error| Error::Schedule(format!("could not run {program}: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    Err(Error::Schedule(match said.trim() {
        "" => format!("{program} {} failed ({})", args[0], output.status),
        said => format!("{program} {} failed: {said}", args[0]),
    }))
}

fn file_error(path: &Path, source: io::Error) -> Error {
    Error::Schedule(format!("cannot write {}: {source}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            program: "/opt/a&b/devdust".into(),
            env: vec![("PATH".into(), "/usr/bin:/opt/100%/bin".into())],
            log: "/home/me/.local/state/devdust/schedule.log".into(),
        }
    }

    #[test]
    fn launchd_job_scans_and_notifies_weekly() {
        let files = Manager::Launchd.files(&job(), Path::new("/home/me"));

        assert_eq!(files.len(), 1);
        let (path, plist) = &files[0];
        assert_eq!(
            path,
            Path::new("/home/me/Library/LaunchAgents/com.devdust.scan.plist")
        );
        assert!(plist.contains("<string>/opt/a&amp;b/devdust</string>"));
        assert!(plist.contains("<string>scan</string>\n        <string>--notify</string>"));
        assert!(plist.contains("<key>Weekday</key>\n        <integer>1</integer>"));
        assert!(!plist.contains("clean"));
    }

    #[test]
    fn systemd_job_scans_and_notifies_weekly() {
        let files = Manager::Systemd.files(&job(), Path::new("/home/me"));

        assert_eq!(
            files[0].0,
            Path::new("/home/me/.config/systemd/user/devdust-scan.service")
        );
        assert!(
            files[0]
                .1
                .contains("ExecStart=\"/opt/a&b/devdust\" scan --notify\n")
        );
        assert!(
            files[0]
                .1
                .contains("Environment=\"PATH=/usr/bin:/opt/100%%/bin\"\n")
        );
        assert!(!files[0].1.contains("clean"));
        assert_eq!(
            files[1].0,
            Path::new("/home/me/.config/systemd/user/devdust-scan.timer")
        );
        assert!(files[1].1.contains("OnCalendar=Mon 10:00\n"));
    }
}
