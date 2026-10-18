//! The config file, defaults, and file locations (design §7).

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::Error;
use crate::rules::{CustomRule, RuleSet};

const DEFAULT_ROOTS: &[&str] = &["Code", "Projects", "src", "dev", "Developer"];

/// The config file as written. Unknown keys are an error, so a typo is not
/// silently ignored.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    roots: Option<Vec<String>>,
    older_than: Option<String>,
    min_size: Option<String>,
    #[serde(default)]
    use_trash: bool,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default)]
    rules: Rules,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Rules {
    #[serde(default)]
    enable: Vec<String>,
    #[serde(default)]
    disable: Vec<String>,
    #[serde(default)]
    custom: Vec<Custom>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Custom {
    dir: String,
    marker: String,
    restore: String,
}

/// The settings of a config file. A setting the file leaves out is `None`,
/// so the command line and the built-in defaults can fill it in.
#[derive(Debug, Default)]
pub struct Config {
    pub roots: Option<Vec<PathBuf>>,
    pub older_than: Option<Duration>,
    pub min_size: Option<u64>,
    pub use_trash: bool,
    /// The ignore patterns as written.
    pub ignore: Vec<String>,
    pub ignore_set: GlobSet,
    pub rules: RuleSet,
}

impl Config {
    /// Reads the config at `path`. A missing file is an empty config.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(Error::ConfigRead {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let home = env::var_os("HOME").map(PathBuf::from);
        Self::parse(&text, home.as_deref()).map_err(|reason| Error::Config {
            path: path.to_path_buf(),
            reason,
        })
    }

    /// Parses config text. `home` replaces a leading `~` in paths.
    pub fn parse(text: &str, home: Option<&Path>) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|error| error.to_string())?;

        let older_than = match &file.older_than {
            Some(text) => Some(
                humantime::parse_duration(text)
                    .map_err(|error| format!("older_than = \"{text}\": {error}"))?,
            ),
            None => None,
        };
        let min_size = match &file.min_size {
            Some(text) => Some(
                text.parse::<bytesize::ByteSize>()
                    .map_err(|error| format!("min_size = \"{text}\": {error}"))?
                    .as_u64(),
            ),
            None => None,
        };

        let mut ignore_set = GlobSetBuilder::new();
        for pattern in &file.ignore {
            let mut glob = expand_home(pattern, home).to_string_lossy().into_owned();
            // A pattern that is not anchored matches at any depth.
            if !glob.starts_with('/') && !glob.starts_with("**") {
                glob = format!("**/{glob}");
            }
            let glob = GlobBuilder::new(&glob)
                .literal_separator(true)
                .build()
                .map_err(|error| format!("ignore pattern \"{pattern}\": {error}"))?;
            ignore_set.add(glob);
        }

        let custom = file.rules.custom.into_iter().map(|rule| CustomRule {
            dir: rule.dir,
            marker: rule.marker,
            restore: rule.restore,
        });
        Ok(Self {
            roots: file
                .roots
                .map(|roots| roots.iter().map(|root| expand_home(root, home)).collect()),
            older_than,
            min_size,
            use_trash: file.use_trash,
            ignore_set: ignore_set.build().map_err(|error| error.to_string())?,
            ignore: file.ignore,
            rules: RuleSet::new(&file.rules.enable, &file.rules.disable, custom.collect())?,
        })
    }
}

fn expand_home(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// The default root folders that exist in the user's home directory.
pub fn default_roots() -> Vec<PathBuf> {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    DEFAULT_ROOTS
        .iter()
        .map(|name| home.join(name))
        .filter(|path| path.is_dir())
        .collect()
}

/// Where the config lives: `$XDG_CONFIG_HOME/devdust/config.toml`, or
/// `~/.config/devdust/config.toml`. An XDG-style path on macOS too.
pub fn config_path() -> Option<PathBuf> {
    Some(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("devdust/config.toml"))
}

/// Where the action log lives: `$XDG_STATE_HOME/devdust/log.jsonl`, or
/// `~/.local/state/devdust/log.jsonl`.
pub fn log_path() -> Option<PathBuf> {
    Some(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("devdust/log.jsonl"))
}

fn xdg_dir(variable: &str, fallback: &str) -> Option<PathBuf> {
    match env::var_os(variable) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => Some(PathBuf::from(env::var_os("HOME")?).join(fallback)),
    }
}
