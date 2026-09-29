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
