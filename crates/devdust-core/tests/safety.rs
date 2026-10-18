//! Property: `clean` deletes exactly the untracked dust of cold projects,
//! and never a tracked file or a manifest (design §10).

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

use devdust_core::execute::{CleanOptions, execute};
use devdust_core::{ScanOptions, scan};
use devdust_fixtures::Fixture;
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
struct Kind {
    marker: &'static str,
    dust: &'static str,
    /// The rule only matches a git-ignored directory.
    needs_ignore: bool,
}

const KINDS: &[Kind] = &[
    Kind {
        marker: "package.json",
        dust: "node_modules",
        needs_ignore: false,
    },
    Kind {
        marker: "Cargo.toml",
        dust: "target",
        needs_ignore: false,
    },
    Kind {
        marker: "go.mod",
        dust: "vendor",
        needs_ignore: false,
    },
    Kind {
        marker: "composer.json",
        dust: "vendor",
        needs_ignore: false,
    },
    Kind {
        marker: "package.json",
        dust: "dist",
        needs_ignore: true,
    },
    Kind {
        marker: "pyproject.toml",
        dust: ".venv",
        needs_ignore: false,
    },
];

#[derive(Debug, Clone)]
struct Spec {
    kind: Kind,
    in_git: bool,
    /// The dust directory is committed.
    tracked: bool,
    ignored: bool,
    /// Last touched 100 days ago, as opposed to just now.
    old: bool,
    /// A tracked source file has an uncommitted change.
    dirty: bool,
}

impl Spec {
    fn should_be_deleted(&self) -> bool {
        let tracked = self.in_git && self.tracked;
        let dirty = self.in_git && self.dirty;
        let matches_rule = !self.kind.needs_ignore || (self.in_git && self.ignored);
        self.old && !tracked && !dirty && matches_rule
    }
}

fn spec() -> impl Strategy<Value = Spec> {
    (0..KINDS.len(), any::<[bool; 5]>()).prop_map(|(kind, flags)| Spec {
        kind: KINDS[kind],
        in_git: flags[0],
        tracked: flags[1],
        ignored: flags[2],
        old: flags[3],
        dirty: flags[4],
    })
}

fn build(fx: &Fixture, name: &str, spec: &Spec) {
    let Kind { marker, dust, .. } = spec.kind;
    let days = if spec.old { 100 } else { 0 };
    fx.file(&format!("{name}/{marker}"), "{}")
        .file(&format!("{name}/src/main.txt"), "source");
    if spec.in_git {
        fx.git_init(name);
        if spec.ignored {
            fx.file(&format!("{name}/.gitignore"), &format!("{dust}/\n"));
        }
        fx.commit_all_at(name, days);
    }
    fx.file(&format!("{name}/{dust}/dep/file.txt"), "generated");
    if spec.in_git && spec.tracked {
        fx.git(name, &["add", "-f", dust])
            .git_at(name, days, &["commit", "-q", "-m", "vendored"]);
    }
    if spec.in_git && spec.dirty {
        fx.file(&format!("{name}/src/main.txt"), "edited");
    }
    if spec.old {
        fx.age(name, 101);
    }
}

fn git_lines(fx: &Fixture, name: &str, args: &[&str]) -> Vec<String> {
    let output = Command::new("git")
        .current_dir(fx.path(name))
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines().map(String::from).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn clean_deletes_only_untracked_dust_of_cold_projects(
        specs in prop::collection::vec(spec(), 1..5)
    ) {
        let (fx, state) = (Fixture::new(), Fixture::new());
        let mut tracked: BTreeSet<PathBuf> = BTreeSet::new();
        for (index, spec) in specs.iter().enumerate() {
            let name = format!("p{index}");
            build(&fx, &name, spec);
            if spec.in_git {
                let files = git_lines(&fx, &name, &["ls-files"]);
                tracked.extend(files.iter().map(|file| fx.path(&name).join(file)));
            }
        }

        let scanned = scan(&ScanOptions::new(vec![fx.root().to_path_buf()]), &()).unwrap();
        let options = CleanOptions {
            trash: false,
            log_path: state.path("log.jsonl"),
            check_open_files: false,
            now: SystemTime::now(),
            only: None,
        };
        execute(&scanned, &options, &|_| {}).unwrap();

        for file in &tracked {
            prop_assert!(file.exists(), "tracked file deleted: {}", file.display());
        }
        for (index, spec) in specs.iter().enumerate() {
            let name = format!("p{index}");
            let Kind { marker, dust, .. } = spec.kind;
            prop_assert!(fx.path(&name).join(marker).exists(), "{name}: manifest deleted");
            prop_assert!(fx.path(&name).join("src/main.txt").exists(), "{name}: source deleted");
            prop_assert_eq!(
                !fx.path(&name).join(dust).exists(),
                spec.should_be_deleted(),
                "{}: {:?}",
                name,
                spec
            );
            if spec.in_git {
                let status = git_lines(&fx, &name, &["status", "--porcelain"]);
                let deleted: Vec<_> = status.iter().filter(|line| line[..2].contains('D')).collect();
                prop_assert!(deleted.is_empty(), "{name}: git sees deletions: {deleted:?}");
            }
        }
    }
}
