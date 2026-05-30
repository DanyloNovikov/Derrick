# syntax=docker/dockerfile:1
#
# Single source of truth for every environment.
#
#   --target dev      → interactive shell (cargo, rustup, Scarb, source bind-mount via compose)
#   --target builder  → compiles the release binary using the SAME toolchain as dev
#   --target runtime  → minimal production image with just the `derrick` binary
#
# All stages descend from the `base` stage, so the rustc/Scarb versions used in
# dev are identical to those used to build the production binary. There is no
# way to ship a binary built against a different toolchain than the one you
# tested with locally.
#
# Build a production image:
#
#     docker build --target runtime -t derrick:latest .
#
# Build the dev image (or use `make build`):
#
#     docker build --target dev -t derrick-dev:latest .
#
# Version bumps in one place:
ARG RUST_VERSION=1.94
ARG SCARB_VERSION=2.16.0
ARG DEBIAN_VARIANT=bookworm

# ─────────────────────────────────────────────────────────────────────────────
# base — common toolchain layer. Anything used by both `cargo build` (in
# builder) and `cargo check`/`cargo test` (in dev) lives here.
# ─────────────────────────────────────────────────────────────────────────────
FROM rust:${RUST_VERSION}-${DEBIAN_VARIANT} AS base

ARG SCARB_VERSION

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        ca-certificates \
        curl \
        unzip \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy

# Scarb (Cairo package manager). The installer exits non-zero with
# "could not detect shell" in non-interactive images even when the binary is
# correctly placed — we accept that benign exit as long as the binary lands.
RUN curl --proto '=https' --tlsv1.2 -sSf https://docs.swmansion.com/scarb/install.sh \
        | sh -s -- -v ${SCARB_VERSION} \
        || test -x /root/.local/bin/scarb
ENV PATH="/root/.local/bin:${PATH}"

ENV CARGO_HOME=/usr/local/cargo
WORKDIR /workspace

# ─────────────────────────────────────────────────────────────────────────────
# dev — interactive development shell. Source is bind-mounted at runtime
# (see docker-compose.yml `dev` service). The image itself stays source-free
# so it doesn't get invalidated every time a .rs file changes.
# ─────────────────────────────────────────────────────────────────────────────
FROM base AS dev

# Mirrors the bind-mount target in docker-compose.yml so `cargo build` inside
# the container writes to the workspace-level cache volume.
ENV CARGO_TARGET_DIR=/workspace/target

CMD ["bash"]

# ─────────────────────────────────────────────────────────────────────────────
# builder — compiles the release binary. Uses BuildKit cache mounts so the
# cargo registry and the target dir survive across rebuilds.
# ─────────────────────────────────────────────────────────────────────────────
FROM base AS builder

# Copy the manifests + sources. We don't try to be clever with a "deps-only"
# layer here — BuildKit's cache mounts give us most of the win with simpler
# Dockerfile semantics.
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build only the bot binary in release mode. The cache mounts mean a second
# `docker build` after touching one file is fast; the final binary is copied
# out of the (ephemeral) cache mount into a stable path before the layer ends.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --release -p bot \
    && cp /workspace/target/release/derrick /usr/local/bin/derrick

# ─────────────────────────────────────────────────────────────────────────────
# runtime — production image. No rustc, no cargo, no source. Just the binary,
# default config, ca-certs (for HTTPS to the RPC endpoint), and tzdata (for
# correct timestamps in tracing logs).
# ─────────────────────────────────────────────────────────────────────────────
FROM debian:${DEBIAN_VARIANT}-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 derrick \
    && useradd --system --uid 10001 --gid derrick --home-dir /app --create-home derrick

WORKDIR /app

COPY --from=builder /usr/local/bin/derrick /usr/local/bin/derrick
COPY config/ /app/config/

USER derrick

# Prometheus exporter (config/default.toml: [observability].metrics_bind).
EXPOSE 9090

# Secrets (OWNER_PRIVATE_KEY — the Oracle wallet's signing key) and per-env
# overrides (DERRICK__...) come from the runtime: Kubernetes / Secrets Manager
# / Compose `environment:`. Never bake them into the image.
ENTRYPOINT ["/usr/local/bin/derrick"]
CMD ["--config", "/app/config/default.toml"]
