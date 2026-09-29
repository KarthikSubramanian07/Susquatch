# Susquatch — one entry point for humans, CI, and the server.
# `make help` lists targets.

SHELL := /bin/bash
.DEFAULT_GOAL := help
UV ?= uv
CARGO ?= cargo
CPP_BUILD ?= build/cpp

.PHONY: help
help: ## List targets
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-18s\033[0m %s\n",$$1,$$2}'

# ── setup ────────────────────────────────────────────────────────────────
.PHONY: setup hooks
setup: hooks ## Install toolchains' deps (uv sync, cargo fetch)
	$(UV) sync
	$(CARGO) fetch

hooks: ## Install the git pre-commit hook (gitleaks + fmt)
	@ln -sf ../../scripts/hooks/pre-commit .git/hooks/pre-commit && echo "pre-commit hook installed"

# ── quality gates ────────────────────────────────────────────────────────
.PHONY: fmt lint test test-rust test-py test-cpp models ci
fmt: ## Format everything
	$(CARGO) fmt --all
	$(UV) run ruff format .
	$(UV) run ruff check --fix .

lint: ## Lint everything (no writes)
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(UV) run ruff check .
	$(UV) run ruff format --check .

test-rust: ## Rust unit + property tests
	$(CARGO) test --workspace

test-py: ## Python tests (never hits the live API)
	$(UV) run pytest -m "not live"

test-cpp: ## Configure, build and run C++ tests
	cmake -S cpp -B $(CPP_BUILD) -DCMAKE_BUILD_TYPE=RelWithDebInfo
	cmake --build $(CPP_BUILD) -j
	ctest --test-dir $(CPP_BUILD) --output-on-failure

models: ## Build the susq_models Python extension (C++ via pybind11) and run its tests
	$(UV) sync --group models
	$(UV) run --group models pytest cpp/tests_py

test: test-rust test-py ## Rust + Python tests

ci: lint test ## What CI runs

# ── contracts ────────────────────────────────────────────────────────────
PROTOS := $(wildcard contracts/proto/susq/v1/*.proto)
.PHONY: contracts contracts-check
contracts: ## Regenerate Python contract types + golden fixtures (Rust/C++ generate at build time)
	protoc -Icontracts/proto --python_out=py --pyi_out=py $(PROTOS)
	$(UV) run python contracts/tools/make_fixtures.py

contracts-check: contracts ## Fail if generated code or fixtures drifted from the schema
	git diff --exit-code -- py/susq/v1 contracts/fixtures

# ── ops ──────────────────────────────────────────────────────────────────
.PHONY: docs-fetch secrets-scan
docs-fetch: ## Snapshot the official API docs (flags changes)
	python3 scripts/fetch_api_docs.py

secrets-scan: ## Full-history secret scan
	gitleaks git --no-banner --redact --config .gitleaks.toml

# ── server (Oracle A1 or the Mac fallback; see docs/OPS.md) ─────────────
COMPOSE := docker compose --project-directory $(CURDIR) -f ops/compose.yaml
# Host-side Python helpers: uv where it exists (Mac, dev), else the stack's own image (server).
OPS_PY = $(if $(shell command -v $(UV)),$(UV) run python -m,$(COMPOSE) run --rm --no-deps -T status-pusher python -m)
.PHONY: up down logs ps costs backup restore selfcheck unkill priority
up: ## Start the stack (profiles from COMPOSE_PROFILES in .env)
ifeq ($(shell uname -s),Darwin)
	ops/launchd/mac-up.sh
else
	$(COMPOSE) up -d --build --remove-orphans
	ops/priority.sh auto
endif

down: ## Stop the stack (ops/KILL is the way to stop trading; this stops everything)
	$(COMPOSE) down

logs: ## Follow logs (S=<service> for one)
	$(COMPOSE) logs -f --tail=100 $(S)

ps: ## Service status + health
	$(COMPOSE) ps

costs: ## Free-tier audit: exits non-zero if anything would cost money
	$(OPS_PY) susq.ops.costs

backup: ## Back up state/journal/data/reports/targets to R2 now
	ops/backup.sh

restore: ## Restore the latest R2 backup (ARGS="--list" / "--archive NAME" / "--no-start")
	ops/restore.sh $(ARGS)

selfcheck: ## Disk, clock, expiries, lease, backups, quotas -> ACTION NEEDED items
	$(OPS_PY) susq.ops.selfcheck

unkill: ## HUMAN ONLY: clear the dashboard Kill flag, then remove ops/KILL (ARGS=--local-only)
	$(OPS_PY) susq.ops.kill_poller unkill $(ARGS)

priority: ## Election-night CPU priority status (MODE=election|normal|auto)
	ops/priority.sh $(or $(MODE),status)

# ── dashboard + daily-summary Worker (Cloudflare; see docs/OPS.md) ──────
.PHONY: dashboard-dev dashboard-test deploy-cloudflare
dashboard-dev: ## Local dashboard on :8788 with seeded local KV (SEED=normal|killed|pending|stale|empty)
	cd dashboard && npm install --no-fund --no-audit && SEED=$(or $(SEED),normal) npm run dev

dashboard-test: ## Typecheck + tests for Pages Functions and the Worker
	cd dashboard && npm ci --no-fund --no-audit && npm run check

deploy-cloudflare: ## Deploy Pages + Worker through the GitHub workflow (one deploy path, no local creds)
	gh workflow run deploy-cloudflare.yml --ref main && gh run list --workflow deploy-cloudflare.yml --limit 1
