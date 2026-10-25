# syntax=docker/dockerfile:1.12
# One image for the graft binary. Legacy role names are symlinks for the dev
# stack and one compatibility release; see docs/dev-stack.md.
#
# Stages: builder (rust, sccache) -> runtime (debian trixie-slim, git and
# ca-certificates, non-root user).
#
# Build caching (WI-2234). There is no cargo-chef stage: the cargo registry,
# git checkouts and target directory are BuildKit cache mounts, so a warm
# builder only recompiles what changed, and a cold one compiles through
# sccache when the build is given its S3 credentials (the CI runners' shared
# Garage bucket, .github/workflows/image.yml). Without them (a local
# `docker compose build`) sccache stays off and the cache mounts alone apply.
#
# Parallelism is not capped here. A shared host passes CARGO_BUILD_JOBS as a
# build arg (compose.yaml does, default 4).

ARG RUST_VERSION=1.90
ARG DEBIAN_VERSION=trixie

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS builder
ARG SCCACHE_VERSION=0.18.0
ADD --checksum=sha256:45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89 \
    https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz \
    /tmp/sccache.tar.gz
RUN tar -xzf /tmp/sccache.tar.gz -C /usr/local/bin --strip-components=1 \
      "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" \
 && rm /tmp/sccache.tar.gz \
 && sccache --version
WORKDIR /src
COPY . .
# Empty means no cap. Non-secret sccache settings come in as build args; the
# S3 keys only as build secrets, so they never reach a layer or the history.
ARG CARGO_BUILD_JOBS=
ARG SCCACHE_BUCKET=
ARG SCCACHE_ENDPOINT=
ARG SCCACHE_REGION=
ARG SCCACHE_S3_USE_SSL=
ENV CARGO_INCREMENTAL=0 \
    SCCACHE_IGNORE_SERVER_IO_ERROR=1
# The binaries are copied out of the target cache mount into /out, which is
# part of this stage's layer, for the runtime stage to copy.
RUN --mount=type=cache,id=graft-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=graft-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=graft-target,target=/src/target,sharing=locked \
    --mount=type=secret,id=sccache_key_id,env=AWS_ACCESS_KEY_ID \
    --mount=type=secret,id=sccache_secret,env=AWS_SECRET_ACCESS_KEY \
    set -eu; unset RUSTC_WRAPPER; \
    [ -n "${CARGO_BUILD_JOBS}" ] || unset CARGO_BUILD_JOBS; \
    if [ -n "${SCCACHE_BUCKET}" ] && [ -n "${AWS_ACCESS_KEY_ID:-}" ] \
       && sccache --start-server; then \
      export RUSTC_WRAPPER=sccache; \
    else \
      echo "sccache: off (no bucket, no credentials, or server did not start)"; \
    fi; \
    cargo build --release --locked -p graft-cli; \
    if [ -n "${RUSTC_WRAPPER:-}" ]; then sccache --show-stats; sccache --stop-server; fi; \
    mkdir -p /out; \
    cp target/release/graft /out/

FROM debian:${DEBIAN_VERSION}-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/*
# The volume mount points (meta, objects, mirror) exist here, owned by graft,
# so a fresh named volume mounted on one is seeded with that owner rather than
# root (the pilot stack, ops repo personal/graft).
# /opt/graft-dev/bin is writable by graft, so `docker compose watch` can sync
# host binaries there. /usr/local/bin stays root-owned. It is first on PATH.
RUN groupadd --system --gid 10001 graft \
 && useradd --system --uid 10001 --gid graft \
      --home-dir /var/lib/graft --shell /usr/sbin/nologin graft \
 && mkdir -p /var/lib/graft/meta /var/lib/graft/objects /var/lib/graft/mirror /var/cache/graft /run/graft /work /etc/graft \
 && mkdir -p /opt/graft-dev/bin \
 && chown -R graft:graft /var/lib/graft /var/cache/graft /run/graft /work /opt/graft-dev

# Dev configs and the fixture entrypoint are baked in, so the stack needs no
# host paths. Rebuild the image after changing them.
COPY docker/dev/server.toml /etc/graft/server.toml
COPY docker/dev/mirror /etc/graft-dev/mirror

COPY --from=builder /out/graft /usr/local/bin/
RUN ln -s graft /usr/local/bin/graft-server \
 && ln -s graft /usr/local/bin/graft-node \
 && ln -s graft /usr/local/bin/graft-mirror

ENV PATH=/opt/graft-dev/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

USER graft:graft
WORKDIR /work
CMD ["graft", "version"]
