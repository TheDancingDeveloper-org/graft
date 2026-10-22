# One image for the graft binaries (graft, graft-server, graft-node,
# graft-mirror). Used by the dev stack in compose.yaml; see docs/dev-stack.md.
#
# Stages: chef (cargo-chef installed) -> planner (recipe) -> builder (deps
# cooked from the recipe, then the workspace) -> runtime (debian trixie-slim,
# git and ca-certificates, non-root user).

ARG RUST_VERSION=1.90
ARG DEBIAN_VERSION=trixie
ARG CARGO_CHEF_VERSION=0.1.71

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS chef
ARG CARGO_CHEF_VERSION
RUN cargo install cargo-chef --version "${CARGO_CHEF_VERSION}" --locked
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
ARG CARGO_BUILD_JOBS=4
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
# Dependencies first: this layer only changes when Cargo.lock or a manifest
# changes, not when workspace sources do.
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked \
      -p graft-cli -p graft-server -p graft-node -p graft-mirror

FROM debian:${DEBIAN_VERSION}-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/*
RUN groupadd --system --gid 10001 graft \
 && useradd --system --uid 10001 --gid graft \
      --home-dir /var/lib/graft --shell /usr/sbin/nologin graft \
 && mkdir -p /var/lib/graft/mirror /var/cache/graft /run/graft /work /etc/graft \
 && chown -R graft:graft /var/lib/graft /var/cache/graft /run/graft /work

COPY --from=builder /src/target/release/graft \
     /src/target/release/graft-server \
     /src/target/release/graft-node \
     /src/target/release/graft-mirror \
     /usr/local/bin/

USER graft:graft
WORKDIR /work
CMD ["graft", "version"]
