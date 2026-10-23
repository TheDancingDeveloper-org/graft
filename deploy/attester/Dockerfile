# syntax=docker/dockerfile:1.12
# The trial attester (WI-2276): graft-attester on a Rust toolchain that can
# build the attested repository. Built from this checkout by deploy/up.sh
# when the `attester` compose profile is on (context: the repository root).
#
#   builder  graft's toolchain builds graft-attester
#   runtime  the attested repository's toolchain (rustTorrent: Rust 1.99,
#            npm for the web UI, clang/lld, OpenSSL headers), plus the
#            attester binary. The attester runs as root with only CHOWN,
#            SETUID, SETGID and KILL, and runs every check as uid 10001.

ARG RUST_VERSION=1.90
ARG RUNTIME_RUST_VERSION=1.99
ARG DEBIAN_VERSION=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS builder
WORKDIR /src
COPY . .
# Empty means no cap; deploy/compose.yml passes the stack's.
ARG CARGO_BUILD_JOBS=
RUN --mount=type=cache,id=graft-attester-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=graft-attester-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=graft-attester-target,target=/src/target,sharing=locked \
    set -eu; \
    [ -n "${CARGO_BUILD_JOBS}" ] || unset CARGO_BUILD_JOBS; \
    cargo build --release --locked -p graft-attester; \
    install -D target/release/graft-attester /out/graft-attester

FROM rust:${RUNTIME_RUST_VERSION}-slim-${DEBIAN_VERSION} AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates git curl pkg-config libssl-dev clang lld npm \
 && rm -rf /var/lib/apt/lists/*
# /var/lib/graft-attester (work, logs, state) belongs to root, the attester.
# /var/cache/graft-attester (cargo home, target dir, HOME) belongs to the
# check user, so a fresh named volume mounted there is seeded with it.
RUN groupadd --system --gid 10001 graft \
 && useradd --system --uid 10001 --gid graft \
      --home-dir /var/cache/graft-attester/home --shell /usr/sbin/nologin graft \
 && mkdir -p /var/lib/graft-attester/work /var/lib/graft-attester/logs \
      /var/lib/graft-attester/state \
      /var/cache/graft-attester/home /var/cache/graft-attester/cargo \
      /var/cache/graft-attester/target \
 && chmod 0755 /var/lib/graft-attester/work \
 && chown -R graft:graft /var/cache/graft-attester
COPY --from=builder /out/graft-attester /usr/local/bin/graft-attester
ENTRYPOINT ["graft-attester"]
