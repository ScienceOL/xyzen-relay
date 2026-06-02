# syntax=docker/dockerfile:1.7
#
# Build all xyzen-relay binaries in one shared cargo-chef pass, then
# split into per-binary runtime images via the `BIN` build-arg. The
# alternative — four independent Dockerfiles — would re-download the
# entire dependency graph four times in CI; this layout shares the
# `cargo chef cook` cache across all four targets.
#
# Build:
#   docker build --build-arg BIN=stream     -t xyzen-relay-stream:dev .
#   docker build --build-arg BIN=control    -t xyzen-relay-control:dev .
#   docker build --build-arg BIN=rendezvous -t xyzen-relay-rendezvous:dev .
#   docker build --build-arg BIN=relay      -t xyzen-relay-relay:dev .
#
# Each binary is a self-contained statically-linked-ish binary (libc
# only) that listens on a single port — see crate-specific README for
# the contract. The runtime image is a minimal debian:bookworm-slim;
# we deliberately do NOT use `scratch` because (a) the binaries link
# against libssl / libcrypto via reqwest's rustls in some crates so
# we need ca-certificates, and (b) `kubectl exec` debugging on a
# running pod is far easier with a real /bin/sh.

ARG RUST_VERSION=1.94

# ─── Plan stage: produce the dependency manifest only ─────────────
FROM rust:${RUST_VERSION}-slim AS planner
WORKDIR /app
RUN cargo install cargo-chef --locked --version 0.1.71
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ─── Build stage: cache compile of every dependency, then sources ─
FROM rust:${RUST_VERSION}-slim AS builder
WORKDIR /app
# `pkg-config` + `libssl-dev` cover any crate that opts out of rustls
# (none today, but keeping the door open is cheaper than a CI
# breakage when someone adds one). protoc is needed by `protox` only
# transitively in dev — we use protox at build-time so no system
# protoc is required.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked --version 0.1.71

COPY --from=planner /app/recipe.json recipe.json
# Cook all dependencies — this is the big cache layer that survives
# every source-only edit.
RUN cargo chef cook --release --recipe-path recipe.json

# Now bring in the actual sources and build every server-side binary.
# We deliberately do NOT include `mac-capturer` or `capturer` here:
# `mac-capturer` is macOS-only (Swift / SCKit) and `capturer` is the
# legacy ffmpeg-subprocess publisher that lives on user machines, not
# in the relay's k8s pods.
COPY . .
RUN cargo build --release \
    --bin xyzen-stream \
    --bin xyzen-control \
    --bin xyzen-rendezvous \
    --bin xyzen-relay

# ─── Runtime: select one binary per image via the BIN build arg ───
FROM debian:bookworm-slim AS runtime
ARG BIN
ENV BIN=${BIN}
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 1000 xyzen \
    && useradd --system --uid 1000 --gid 1000 --shell /usr/sbin/nologin xyzen

# Map the BIN arg to the actual cargo target name. Cargo binaries are
# `xyzen-{stream,control,rendezvous,relay}` and we copy whichever the
# caller asked for.
COPY --from=builder /app/target/release/xyzen-${BIN} /usr/local/bin/xyzen-server
USER xyzen
# Each binary reads its bind config from CLI flags / env. We don't
# pin a port via EXPOSE because the four binaries listen on different
# ports — k8s Service handles port mapping anyway.
ENTRYPOINT ["/usr/local/bin/xyzen-server"]
