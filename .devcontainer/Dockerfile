# Debian trixie with stable Rust, matching the builder in the root Dockerfile.
FROM rust:1-slim-trixie

ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=$USER_UID

# Build deps (same as CI / root Dockerfile) plus the bare minimum for a dev shell.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        less \
        procps \
        pkg-config \
        libssl-dev \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy

# Non-root user that owns the Rust toolchain, so `cargo install` / `rustup` work.
# Pre-create the volume mount points so named volumes inherit this ownership.
RUN groupadd --gid $USER_GID $USERNAME \
    && useradd --uid $USER_UID --gid $USER_GID -m -s /bin/bash $USERNAME \
    && mkdir -p "$CARGO_HOME/registry" /workspace-target \
    && chown -R $USERNAME:$USERNAME "$CARGO_HOME" "$RUSTUP_HOME" /workspace-target

USER $USERNAME

ENV CARGO_TARGET_DIR=/workspace-target
