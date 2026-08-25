CARGO_MANIFEST := Cargo.toml
TAURI_CONF := crates/rezon-web/tauri.conf.json

# The DMG bundler styles its Finder window by driving Finder over Apple
# events, which needs Automation permission the terminal usually does not
# have -- it dies with "Not authorized to send Apple events to Finder.
# (-1743)". Tauri passes bundle_dmg.sh --skip-jenkins whenever CI is set,
# which skips the styling and yields a plain but fully functional image.
# Grant the terminal Automation access to Finder under System Settings >
# Privacy & Security > Automation and run `make dmg DMG_STYLE=1` to get
# the styled window back.
#
# Scoped to the `dmg` target alone so a plain `make build` neither needs
# the permission nor leaks CI=true into the frontend build.
DMG_ENV := $(if $(DMG_STYLE),,CI=true)

.PHONY: help install dev build dmg build-tui build-tui-release run-tui run-tui-release \
		web-dev web-build typecheck check fmt fmt-check lint test ci clean

help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  %-14s %s\n", $$1, $$2}'

install: ## Install JS deps
	@bun install

dev: ## Run Tauri app in dev mode
	@bun run tauri dev --config $(TAURI_CONF)

build: ## Build Rezon.app for release
	@bun run tauri build --config $(TAURI_CONF) --bundles app

# Asks for `app` alongside `dmg` deliberately. With `--bundles dmg` alone
# Tauri treats Rezon.app as a disposable intermediate and deletes it on
# the way out, so `make build && make dmg` would leave no .app behind.
dmg: ## Build the release DMG (DMG_STYLE=1 keeps DMG window styling)
	@$(DMG_ENV) bun run tauri build --config $(TAURI_CONF) --bundles app,dmg

build-tui: ## build tui (debug)
	@cargo build -p rezon-tui

build-tui-release: ## build tui (release)
	@cargo build -p rezon-tui --release

run-tui: ## run tui (debug). Pass args via ARGS="..."
	@cargo run -p rezon-tui -- $(ARGS)

run-tui-release: ## run tui (release). Pass args via ARGS="..."
	@cargo run -p rezon-tui --release -- $(ARGS)

web-dev: ## Run Vite dev server only (no Tauri)
	@bun run dev

web-build: ## Build frontend only
	@bun run build

check: ## cargo check (workspace)
	@cargo check --workspace

fmt: ## Format Rust code (workspace)
	@cargo fmt --all

fmt-check: ## Verify Rust formatting (workspace)
	@cargo fmt --all -- --check

lint: ## Clippy with warnings as errors (workspace)
	@cargo clippy --workspace --all-targets -- -D warnings

test: ## Rust gate: fmt-check + tests + clippy (warnings = errors)
	@cargo fmt --all -- --check
	@cargo test --workspace
	@cargo clippy --workspace --all-targets -- -D warnings

typecheck: ## Typecheck the frontend (needs `make install` first)
	@bunx tsc --noEmit

ci: ## Everything CI runs: Rust gate + frontend typecheck + frontend build
	@$(MAKE) test
	@$(MAKE) typecheck
	@$(MAKE) web-build

clean: ## Remove build artifacts
	@rm -rf node_modules dist target crates/rezon-web/target
