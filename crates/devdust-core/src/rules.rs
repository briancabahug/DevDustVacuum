//! Dust rule table (design §4).

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Rule {
    pub id: &'static str,
    pub ecosystem: &'static str,
    /// Directory names, or relative paths below the marker's folder (`vendor/bundle`).
    pub dirs: &'static [&'static str],
    /// Sibling markers, of which one must exist. A `*` matches any run of
    /// characters. Empty means the rule needs no marker.
    pub markers: &'static [&'static str],
    /// The directory must be git-ignored.
    pub requires_ignored: bool,
    /// The marker must be a `pubspec.yaml` that depends on Flutter.
    pub requires_flutter: bool,
    /// Off unless the config enables it.
    pub opt_in: bool,
    pub restore: Restore,
}

#[derive(Debug)]
pub enum Restore {
    Fixed(&'static str),
    /// Chosen from the lockfile.
    Node,
    /// Chosen from the lockfile or manifest.
    Python,
    /// `flutter pub get` or `dart pub get`.
    Dart,
}

const fn rule(
    id: &'static str,
    ecosystem: &'static str,
    dirs: &'static [&'static str],
    markers: &'static [&'static str],
    restore: Restore,
) -> Rule {
    Rule {
        id,
        ecosystem,
        dirs,
        markers,
        requires_ignored: false,
        requires_flutter: false,
        opt_in: false,
        restore,
    }
}

const PACKAGE_JSON: &[&str] = &["package.json"];
const PUBSPEC: &[&str] = &["pubspec.yaml"];

/// Rules are tried in order; the first one whose directory name and marker
/// both match wins.
pub static RULES: &[Rule] = &[
    rule(
        "node",
        "Node.js",
        &["node_modules"],
        PACKAGE_JSON,
        Restore::Node,
    ),
    rule(
        "js-framework",
        "Node.js",
        &[".next", ".nuxt", ".svelte-kit", ".turbo", ".parcel-cache"],
        PACKAGE_JSON,
        Restore::Fixed("npm run build"),
    ),
    rule(
        "composer",
        "PHP",
        &["vendor"],
        &["composer.json"],
        Restore::Fixed("composer install"),
    ),
    rule(
        "bundler",
        "Ruby",
        &["vendor/bundle"],
        &["Gemfile"],
        Restore::Fixed("bundle install"),
    ),
    // A committed vendor/ is rejected by the tracked-file check every rule gets.
    rule(
        "go",
        "Go",
        &["vendor"],
        &["go.mod"],
        Restore::Fixed("go mod vendor"),
    ),
    rule(
        "rust",
        "Rust",
        &["target"],
        &["Cargo.toml"],
        Restore::Fixed("cargo build"),
    ),
    rule(
        "maven",
        "Java/Kotlin",
        &["target"],
        &["pom.xml"],
        Restore::Fixed("mvn package"),
    ),
    rule(
        "gradle",
        "Java/Kotlin",
        &["build", ".gradle"],
        &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        Restore::Fixed("./gradlew build"),
    ),
    rule(
        "python",
        "Python",
        &[".venv", "venv"],
        &["pyproject.toml", "requirements*.txt", "setup.py", "Pipfile"],
        Restore::Python,
    ),
    rule(
        "python-test-envs",
        "Python",
        &[".tox", ".nox"],
        &["pyproject.toml", "requirements*.txt", "setup.py", "Pipfile"],
        Restore::Fixed("recreated by the next tox / nox run"),
    ),
    rule(
        "python-cache",
        "Python",
        &["__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache"],
        &[],
        Restore::Fixed("recreated automatically"),
    ),
    rule(
        "dotnet",
        ".NET",
        &["bin", "obj"],
        &["*.csproj", "*.fsproj", "*.sln"],
        Restore::Fixed("dotnet build"),
    ),
    rule(
        "cocoapods",
        "iOS",
        &["Pods"],
        &["Podfile"],
        Restore::Fixed("pod install"),
    ),
    rule(
        "elixir",
        "Elixir",
        &["_build", "deps"],
        &["mix.exs"],
        Restore::Fixed("mix deps.get"),
    ),
    rule(
        "dart",
        "Dart/Flutter",
        &[".dart_tool"],
        PUBSPEC,
        Restore::Dart,
    ),
    // Riskier than the rules below it, which insist on a git-ignored
    // directory: when enabled it takes `dist` and `build` either way.
    Rule {
        opt_in: true,
        ..rule(
            "unignored-build",
            "Build output",
            &["dist", "build"],
            &["package.json", "pubspec.yaml"],
            Restore::Fixed("recreated by the next build"),
        )
    },
    Rule {
        requires_ignored: true,
        ..rule(
            "dart-build",
            "Dart/Flutter",
            &["build"],
            PUBSPEC,
            Restore::Fixed("recreated by the next build"),
        )
    },
    Rule {
        requires_flutter: true,
        ..rule(
            "flutter-platform",
            "Dart/Flutter",
            &[
                "ios/.symlinks",
                "ios/Flutter/ephemeral",
                "macos/Flutter/ephemeral",
                "linux/flutter/ephemeral",
                "windows/flutter/ephemeral",
            ],
            PUBSPEC,
            Restore::Fixed("flutter pub get"),
        )
    },
    rule(
        "terraform",
        "Terraform",
        &[".terraform"],
        &["*.tf"],
        Restore::Fixed("terraform init"),
    ),
    Rule {
        requires_ignored: true,
        ..rule(
            "js-build",
            "Node.js",
            &["dist", "build", "coverage"],
            PACKAGE_JSON,
            Restore::Fixed("npm run build"),
        )
    },
];

fn marker_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((prefix, suffix)) => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
        None => pattern == name,
    }
}

/// Whether any of `names` satisfies one of `markers`.
pub fn has_marker(markers: &[&str], names: &[String]) -> bool {
    names
        .iter()
        .any(|name| markers.iter().any(|m| marker_matches(m, name)))
}

/// The rules a scan uses: the built-in table adjusted by the config, with
/// the config's custom rules in front so that they win.
#[derive(Debug, Clone)]
pub struct RuleSet {
    rules: Vec<&'static Rule>,
}

impl Default for RuleSet {
    fn default() -> Self {
        Self {
            rules: RULES.iter().filter(|rule| !rule.opt_in).collect(),
        }
    }
}

/// A rule from the config file.
pub struct CustomRule {
    pub dir: String,
    pub marker: String,
    pub restore: String,
}

impl RuleSet {
    /// `enable` and `disable` hold ids of built-in rules.
    pub fn new(
        enable: &[String],
        disable: &[String],
        custom: Vec<CustomRule>,
    ) -> Result<Self, String> {
        for id in enable.iter().chain(disable) {
            if !RULES.iter().any(|rule| rule.id == id) {
                let known: Vec<&str> = RULES.iter().map(|rule| rule.id).collect();
                return Err(format!(
                    "unknown rule \"{id}\"; the rules are: {}",
                    known.join(", ")
                ));
            }
        }
        let mut rules = Vec::new();
        for rule in custom {
            rules.push(custom_rule(rule)?);
        }
        rules.extend(RULES.iter().filter(|rule| {
            let enabled = !rule.opt_in || enable.iter().any(|id| id == rule.id);
            enabled && !disable.iter().any(|id| id == rule.id)
        }));
        Ok(Self { rules })
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static Rule> + '_ {
        self.rules.iter().copied()
    }

    /// Whether any of `names` is a marker of any rule in the set.
    pub fn has_manifest(&self, names: &[String]) -> bool {
        self.iter().any(|rule| has_marker(rule.markers, names))
    }
}

/// Checks a custom rule and turns it into a `Rule`. The few strings it holds
/// are leaked: rules live for the whole run, like the built-in table.
fn custom_rule(custom: CustomRule) -> Result<&'static Rule, String> {
    let CustomRule {
        dir,
        marker,
        restore,
    } = custom;
    let parts: Vec<&str> = dir.split('/').collect();
    if parts
        .iter()
        .any(|part| matches!(*part, "" | "." | ".." | ".git"))
    {
        return Err(format!(
            "custom rule dir \"{dir}\" must be a directory name or a relative path below the marker"
        ));
    }
    // A marker is what makes a directory provably regenerable, so a custom
    // rule cannot go without one.
    if marker.is_empty() || marker.contains('/') {
        return Err(format!(
            "custom rule \"{dir}\" needs a marker: the name of a file next to the directory"
        ));
    }
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let id = leak(format!("custom:{dir}"));
    Ok(Box::leak(Box::new(rule(
        id,
        "Custom",
        Box::leak(Box::new([leak(dir)])),
        Box::leak(Box::new([leak(marker)])),
        Restore::Fixed(leak(restore)),
    ))))
}

/// The manifest in `dir`, if any, that opts its project out of cleaning:
/// `"devdust": {"keep": true}` in `package.json`, and the equivalent place
/// for tool settings in the other manifests.
pub fn keep_manifest(dir: &Path) -> Option<PathBuf> {
    const JSON: &[(&str, &[&str])] = &[
        ("package.json", &["devdust", "keep"]),
        ("composer.json", &["extra", "devdust", "keep"]),
    ];
    const TOML: &[(&str, &[&str])] = &[
        ("Cargo.toml", &["package", "metadata", "devdust", "keep"]),
        ("Cargo.toml", &["workspace", "metadata", "devdust", "keep"]),
        ("pyproject.toml", &["tool", "devdust", "keep"]),
    ];
    for (name, keys) in JSON {
        let path = dir.join(name);
        let Some(mut value) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        else {
            continue;
        };
        if keys.iter().all(|key| {
            value = value.get(key).cloned().unwrap_or_default();
            !value.is_null()
        }) && value == true
        {
            return Some(path);
        }
    }
    for (name, keys) in TOML {
        let path = dir.join(name);
        let Some(table) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.parse::<toml::Table>().ok())
        else {
            continue;
        };
        let mut value = Some(&toml::Value::Table(table));
        for key in *keys {
            value = value.and_then(|value| value.get(key));
        }
        if value.and_then(toml::Value::as_bool) == Some(true) {
            return Some(path);
        }
    }
    None
}

/// Names of the non-directory entries of `dir`. Empty if it cannot be read.
pub fn file_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// Whether the `pubspec.yaml` in `dir` depends on the Flutter SDK.
pub fn depends_on_flutter(dir: &Path) -> bool {
    fs::read_to_string(dir.join("pubspec.yaml"))
        .is_ok_and(|text| text.lines().any(|line| line.trim() == "sdk: flutter"))
}

/// The command that regenerates a dust directory whose marker is in `marker_dir`.
/// Lockfiles are looked for from `marker_dir` up to `boundary`, because a
/// workspace keeps its lockfile at the workspace root.
pub fn restore_hint(rule: &Rule, marker_dir: &Path, boundary: &Path) -> String {
    let hint = match rule.restore {
        Restore::Fixed(hint) => hint,
        Restore::Node => node_restore_hint(marker_dir, boundary),
        Restore::Python => {
            let has = |name: &str| marker_dir.join(name).exists();
            if has("uv.lock") {
                "uv sync"
            } else if has("poetry.lock") {
                "poetry install"
            } else if has("Pipfile") {
                "pipenv install"
            } else if has("requirements.txt") {
                "pip install -r requirements.txt"
            } else {
                "pip install -e ."
            }
        }
        Restore::Dart => {
            if depends_on_flutter(marker_dir) {
                "flutter pub get"
            } else {
                "dart pub get"
            }
        }
    };
    hint.to_string()
}

fn node_restore_hint(marker_dir: &Path, boundary: &Path) -> &'static str {
    const LOCKFILES: &[(&str, &str)] = &[
        ("pnpm-lock.yaml", "pnpm install"),
        ("yarn.lock", "yarn install"),
        ("bun.lock", "bun install"),
        ("bun.lockb", "bun install"),
        ("package-lock.json", "npm ci"),
    ];
    for dir in marker_dir.ancestors() {
        for (lockfile, command) in LOCKFILES {
            if dir.join(lockfile).exists() {
                return command;
            }
        }
        if dir == boundary {
            break;
        }
    }
    "npm install"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_patterns() {
        assert!(marker_matches("package.json", "package.json"));
        assert!(!marker_matches("package.json", "package.json5"));
        assert!(marker_matches("*.csproj", "App.csproj"));
        assert!(!marker_matches("*.csproj", "App.csproj.user"));
        assert!(marker_matches("requirements*.txt", "requirements.txt"));
        assert!(marker_matches("requirements*.txt", "requirements-dev.txt"));
        assert!(!marker_matches("requirements*.txt", "requirements"));
    }

    #[test]
    fn rule_dirs_are_relative_and_normalized() {
        for rule in RULES {
            for dir in rule.dirs {
                assert!(!dir.is_empty() && !dir.starts_with('/') && !dir.ends_with('/'));
                assert!(dir.split('/').all(|part| !part.is_empty() && part != ".."));
            }
        }
    }
}
