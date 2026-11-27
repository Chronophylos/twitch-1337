# Dockerfile for twitch-1337 bot
# Uses cargo-chef for efficient dependency caching
# Final image is FROM scratch with statically linked musl binary

# Tag + digest so Dependabot can bump the hash when the chef image for
# this Rust version rebuilds. rust-toolchain.toml is the real pin — it is
# copied in below so every stage resolves the same toolchain regardless of
# which Rust the base image ships.
FROM docker.io/lukemathwalker/cargo-chef:latest-rust-1.100.0@sha256:edbf6876f6a78e921b94e65001cc494214bfb790f374c8bd7bc05de62f69258b AS base

WORKDIR /app
# Before `rustup target add`: the musl std must land on the pinned toolchain,
# not whatever the base image defaults to.
COPY rust-toolchain.toml .
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=clang
ENV RUSTFLAGS="-C link-arg=-fuse-ld=mold"
# hadolint ignore=DL3008
RUN apt-get update \
  && apt-get install --no-install-recommends --assume-yes musl-tools mold clang ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && rustup target add x86_64-unknown-linux-musl

# cargo-auditable embeds the resolved dependency tree in a `.dep-v0` section
# of the binary so Trivy/Grype can scan the FROM scratch image. Installed in
# the base stage (before any source COPY) so its layer stays cached.
# Bump CARGO_AUDITABLE_VERSION deliberately; --locked pins its own deps.
ARG CARGO_AUDITABLE_VERSION=0.7.8
RUN cargo install --locked cargo-auditable --version "${CARGO_AUDITABLE_VERSION}" \
  && rm -rf /usr/local/cargo/registry

# 1. Planner stage - generates dependency recipe
FROM base AS planner
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# 2. Cacher stage - builds dependencies only
FROM base AS cacher

COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --locked --release --target x86_64-unknown-linux-musl --recipe-path recipe.json

# 3. Builder stage - builds the application
FROM base AS builder

# Runtime UID for the scratch image (matches distroless nonroot convention).
RUN groupadd --system --gid 65532 app \
  && useradd --system --uid 65532 --gid app --no-create-home --shell /usr/sbin/nologin app

# Build-arg: short commit SHA of the source tree. Required because
# .dockerignore strips .git/, so the web crate's build.rs cannot derive
# it itself. Defaults to "unknown" if the caller does not pass one.
ARG GIT_SHA=unknown
ENV GIT_SHA=${GIT_SHA}

# Build-arg: monotonic build number (GitHub Actions run number). Surfaced
# in the dashboard sidebar and startup log so the operator can see which
# CI run produced the running image. Defaults to "dev" for local builds.
ARG BUILD_NUM=dev
ENV BUILD_NUM=${BUILD_NUM}

# Copy over cached dependencies
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo

# Copy source code and embedded data
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates crates

# Build the application with musl target (fully static binary,
# dependency tree embedded by cargo-auditable)
RUN cargo auditable build --locked -p twitch-1337 --release --target x86_64-unknown-linux-musl

# 4. Runtime stage - minimal FROM scratch image
FROM scratch

# Re-declare build-args here — ARG scope does not cross FROM.
# Builder stage still owns ENV GIT_SHA/BUILD_NUM for the binary;
# these feed OCI labels only.
ARG GIT_SHA=unknown
ARG BUILD_NUM=dev
ARG CREATED=

LABEL org.opencontainers.image.title="twitch-1337" \
  org.opencontainers.image.description="A Twitch bot for one channel, with a mind of its own: the 13:37 race, AI chat, flight tracking, alerts and a web dashboard. One static Rust binary, FROM scratch." \
  org.opencontainers.image.source="https://github.com/Chronophylos/twitch-1337" \
  org.opencontainers.image.url="https://github.com/Chronophylos/twitch-1337" \
  org.opencontainers.image.documentation="https://github.com/Chronophylos/twitch-1337#readme" \
  org.opencontainers.image.licenses="MIT OR Apache-2.0" \
  org.opencontainers.image.authors="https://github.com/Chronophylos" \
  org.opencontainers.image.vendor="Chronophylos" \
  org.opencontainers.image.revision="${GIT_SHA}" \
  org.opencontainers.image.version="${BUILD_NUM}" \
  org.opencontainers.image.created="${CREATED}"

ENV DATA_DIR=/data

# Copy CA bundle so rustls-platform-verifier can load system roots
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy the static binary
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/twitch-1337 /twitch-1337

USER 65532:65532

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD ["/twitch-1337", "--healthcheck"]

# Run the bot
ENTRYPOINT ["/twitch-1337"]
