# DevDustVacuum

`devdust` finds regenerable dependency and build directories (`node_modules`, `target`, `.venv`, `vendor`, Flutter `build`, and others) under your code folders, works out which projects are cold, and deletes their dust when you tell it to. Everything it removes can be regenerated with the project's own install or build command.

## Install

You need [Rust](https://rustup.rs) (stable) and `git`.

```sh
cargo install --path crates/devdust
```

This puts `devdust` in `~/.cargo/bin`.

### Run without installing

You can run `devdust` from this checkout without installing it. `make run` builds it and passes `ARGS` through:

```sh
make run ARGS="scan ~/Code --older-than 60d"
make scan ROOTS="~/Code ~/Projects"    # read-only scan
make vacuum ROOTS="~/Code"             # dry run of devdust clean on those folders
make vacuum ROOTS="~/Code" APPLY=1     # really delete (asks first); more flags go in ARGS
make release && ./target/release/devdust scan
```

`make help` lists every target. If you set up the weekly scan this way (`make run ARGS="schedule install"`), the job runs `target/release/devdust`, so it stops working after `make clean`. `make clean` only deletes this repo's own `target/` folder. To clean your projects' dust, use `make vacuum`.

## Use

### Scan

`scan` is read-only.

```sh
devdust scan                      # scans the roots in your config file, or the default folders
devdust scan ~/Work ~/Code        # scans the folders you name
devdust scan --older-than 90d     # cold means untouched for 90 days (default 30d)
devdust scan --min-size 100MB     # hides projects with less dust than this
devdust scan --cross-devices      # also scans other volumes mounted below the folders
devdust scan --json               # machine-readable output
```

The default folders are `~/Code`, `~/Projects`, `~/src`, `~/dev`, and `~/Developer`; the ones that don't exist are left out.

Example output:

```
 PROJECT             BRANCH  AGE   DUST                 SIZE
 ~/Code/shop-app     main    214d  node_modules, .next  1.4 GB

1 cold project · 1.4 GB reclaimable

Kept (1 project, 568 MB):
 PROJECT             BRANCH  WHY                        DUST    SIZE
 ~/Code/rust-tool    main    commit or checkout 2d ago  target  568 MB
```

A directory only counts as dust when its manifest is next to it (for example `node_modules` beside `package.json`) and git tracks no file inside it. Directories that were rejected are listed under "Skipped" with the reason.

A project is cold when nothing shows activity within the threshold: no commit or checkout in its git worktree, no uncommitted changes, no edited file, and no rebuilt dust. Each git worktree is judged on its own. Projects in a declared workspace are judged together; `--group-by repo` or `--group-by project` changes that.

### Clean

```sh
devdust clean                     # dry run: shows what would be deleted
devdust clean --apply             # asks, then deletes the dust of cold projects
devdust clean --apply --yes       # deletes without asking
devdust clean --apply -i          # lets you tick the projects to clean in a list
devdust clean --apply --trash     # moves the dust to the Trash instead
```

`clean` takes the same folders and `--older-than`, `--min-size`, `--group-by`, and `--cross-devices` options as `scan`. Right before removing each directory it checks again that the project is still cold, the manifest is still there, git tracks nothing inside, and no running process has files open in it.

Every removed directory is recorded in `~/.local/state/devdust/log.jsonl` with the command that restores it (for example `npm ci` or `cargo build`). `devdust restore-hints` lists the removed directories that are still missing, with those commands:

```sh
devdust restore-hints                  # everything still missing
devdust restore-hints ~/Code/shop-app  # only what was removed inside this folder
```

Locked git worktrees and worktrees outside the scanned folders are reported but left alone, unless you pass `--include-locked` or `--include-external-worktrees`.

Run `devdust scan --help` or `devdust clean --help` for all options.

### Configure

Settings live in `~/.config/devdust/config.toml`. Every one is optional, and a command-line flag wins over the file.

```toml
roots = ["~/Code", "~/Work"]
older_than = "30d"
min_size = "20MB"
use_trash = false

# Paths that are never searched.
ignore = ["~/Code/client-x/**", "**/fixtures/**"]

[rules]
disable = ["dotnet"]            # turn a built-in rule off
enable = ["unignored-build"]    # dist/ and build/ that are not git-ignored

# Your own rule: a directory, the file that must sit next to it, and how to rebuild it.
[[rules.custom]]
dir = ".cache-build"
marker = "Makefile"
restore = "make"
```

`devdust config` shows where the file is and the settings in effect; `devdust config --edit` opens it in your editor.

To keep one project's dust for good, put an empty `.devdustkeep` file in its folder, or set `"devdust": { "keep": true }` in its `package.json` (`Cargo.toml`, `pyproject.toml`, and `composer.json` have an equivalent; see [docs/design.md](docs/design.md)).

### Schedule

```sh
devdust schedule install          # scans every Monday at 10:00 and notifies you
devdust schedule status
devdust schedule remove
```

The scheduled job runs `devdust scan --notify` with the roots and settings from your config file. It shows a desktop notification such as "5.1 GB of dev dust in 3 cold projects" when there is something to reclaim, and never deletes anything. It is a launchd agent on macOS and a systemd user timer on Linux; `devdust schedule install --print` shows the files without installing them.

The job runs the `devdust` binary you installed it from, so run `devdust schedule install` again if that binary moves.

### Shell completions

`devdust completions <shell>` prints a completion script for `bash`, `zsh`, `fish`, `elvish`, or `powershell`. For zsh:

```sh
devdust completions zsh > ~/.zfunc/_devdust    # a folder on your $fpath
```

## Develop

```sh
make help     # list every target
make build    # build the workspace
make check    # format check, clippy, and tests
```

## License

MIT. See [LICENSE](LICENSE).
