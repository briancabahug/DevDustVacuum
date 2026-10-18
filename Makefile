# DevDustVacuum
#
# Run `make` or `make help` to list the targets.
#
# Run devdust straight from this checkout, without `cargo install`:
#
#   make run ARGS="scan ~/Code --older-than 60d"
#   make scan ROOTS="~/Code ~/Projects"
#   make vacuum ROOTS="~/Code" APPLY=1
#   make release && ./target/release/devdust scan
#
# Written for the GNU Make 3.81 that ships with macOS.

CARGO   ?= cargo
PROFILE ?= release
ARGS    ?=
ROOTS   ?=
APPLY   ?=

ifeq ($(PROFILE),release)
  PROFILE_FLAG := --release
else
  PROFILE_FLAG :=
endif

RUN := $(CARGO) run --quiet $(PROFILE_FLAG) -p devdust --

ifeq ($(APPLY),1)
  APPLY_FLAG := --apply
else
  APPLY_FLAG :=
endif

.DEFAULT_GOAL := help

.PHONY: help run scan vacuum restore-hints release build fmt fmt-check lint test check install uninstall clean

help: ## Show this list of targets
	@echo "Usage: make <target> [ARGS=\"...\"] [ROOTS=\"...\"] [APPLY=1] [PROFILE=release|dev]"
	@echo
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN { FS = ":.*## " } { printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2 }'
	@echo
	@echo "Examples:"
	@echo "  make run ARGS=\"--help\"                    # any devdust command, run from this checkout"
	@echo "  make scan ROOTS=\"~/Code ~/Projects\"       # scan chosen folders"
	@echo "  make vacuum ROOTS=\"~/Code ~/Projects\"     # dry run: show the dust it would delete there"
	@echo "  make vacuum ROOTS=\"~/Code\" APPLY=1        # really delete it (asks first)"
	@echo "  make vacuum ROOTS=\"~/Code\" APPLY=1 ARGS=\"--older-than 7d --trash\""
	@echo "  make run PROFILE=dev ARGS=\"scan\"          # faster to compile, slower to scan"

## --- Run without installing -------------------------------------------------

run: ## Build and run devdust from this checkout; pass its arguments in ARGS
	$(RUN) $(ARGS)

scan: ## Read-only scan of ROOTS (default: your config's roots)
	$(RUN) scan $(ROOTS)

vacuum: ## Clean the dust of cold projects in ROOTS; a dry run unless APPLY=1. Extra flags go in ARGS
	$(RUN) clean $(APPLY_FLAG) $(ARGS) $(ROOTS)

restore-hints: ## Show how to bring back directories devdust removed
	$(RUN) restore-hints

release: ## Build the optimised binary at target/release/devdust
	$(CARGO) build --release -p devdust
	@echo "Built $(CURDIR)/target/release/devdust"

## --- Develop -----------------------------------------------------------------

build: ## Build the whole workspace (debug)
	$(CARGO) build --workspace

fmt: ## Format all code with rustfmt
	$(CARGO) fmt --all

fmt-check: ## Fail if any code is not formatted
	$(CARGO) fmt --all --check

lint: ## Run clippy on all targets, treating warnings as errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test: ## Run every test in the workspace
	$(CARGO) test --workspace

check: fmt-check lint test ## Format check, clippy, and tests (what CI should run)

## --- Install -----------------------------------------------------------------

install: ## Install devdust into ~/.cargo/bin (optional; `make run` does not need it)
	$(CARGO) install --path crates/devdust

uninstall: ## Remove devdust from ~/.cargo/bin
	$(CARGO) uninstall devdust

clean: ## Delete this repo's target/ build directory (not your projects' dust; see vacuum)
	$(CARGO) clean
