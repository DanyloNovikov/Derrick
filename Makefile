.PHONY: help build up down psql shell check fmt fmt-check clippy test

DC  := docker compose
DEV := $(DC) --profile dev run --rm dev

help:
	@echo "Targets:"
	@echo "  make build       - build the dev container image"
	@echo "  make up          - start postgres in background"
	@echo "  make down        - stop and remove all containers"
	@echo "  make psql        - open psql in the postgres container"
	@echo "  make shell       - open bash inside the dev container"
	@echo "  make check       - cargo check --workspace --all-targets"
	@echo "  make fmt         - cargo fmt --all"
	@echo "  make fmt-check   - cargo fmt --all -- --check"
	@echo "  make clippy      - cargo clippy --workspace --all-targets -- -D warnings"
	@echo "  make test        - cargo test --workspace"

build:
	$(DC) --profile dev build dev

up:
	$(DC) up -d postgres

down:
	$(DC) down

psql:
	$(DC) exec postgres psql -U derrick -d derrick

shell:
	$(DEV) bash

check:
	$(DEV) cargo check --workspace --all-targets

fmt:
	$(DEV) cargo fmt --all

fmt-check:
	$(DEV) cargo fmt --all -- --check

clippy:
	$(DEV) cargo clippy --workspace --all-targets -- -D warnings

test:
	$(DEV) cargo test --workspace

cairo-build:
	$(DEV) bash -c "cd contracts/executor && scarb build"

cairo-clean:
	$(DEV) bash -c "cd contracts/executor && scarb clean"

shell-cairo:
	$(DEV) bash -c "cd contracts/executor && bash"
