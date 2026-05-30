.PHONY: help build image down shell check fmt fmt-check clippy test

DC  := docker compose
DEV := $(DC) --profile dev run --rm dev

# Tag for the production runtime image. Override on the command line:
#   make image IMAGE_TAG=derrick:0.1.0
IMAGE_TAG ?= derrick:latest

help:
	@echo "Targets:"
	@echo "  make build       - build the dev container image (target=dev)"
	@echo "  make image       - build the production image (target=runtime, tag $$IMAGE_TAG)"
	@echo "  make down        - stop and remove all containers"
	@echo "  make shell       - open bash inside the dev container"
	@echo "  make check       - cargo check --workspace --all-targets"
	@echo "  make fmt         - cargo fmt --all"
	@echo "  make fmt-check   - cargo fmt --all -- --check"
	@echo "  make clippy      - cargo clippy --workspace --all-targets -- -D warnings"
	@echo "  make test        - cargo test --workspace"

build:
	$(DC) --profile dev build dev

# Production image — same Dockerfile, same toolchain stage as `dev`, but the
# `runtime` target strips everything except the compiled binary.
image:
	DOCKER_BUILDKIT=1 docker build --target runtime -t $(IMAGE_TAG) .

down:
	$(DC) down

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

# ─── admin CLI ───────────────────────────────────────────────────────────
# Quick wrappers around `derrick-admin` for common ops. Honour env
# OWNER_PRIVATE_KEY + DERRICK__* the same way the bot does.

admin-build:
	$(DEV) cargo build -p admin-cli --release

admin-status:
	$(DEV) cargo run -q -p admin-cli -- status

admin-setup:
	$(DEV) cargo run -q -p admin-cli -- setup

admin-setup-dry:
	$(DEV) cargo run -q -p admin-cli -- setup --dry-run
