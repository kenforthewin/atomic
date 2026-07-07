# syntax=docker/dockerfile:1
# =============================================================================
# Atomic Cloud pod image — the single `serve` pod described in
# crates/atomic-cloud/DEPLOY.md.
#
# One image carries everything the pod needs:
#   - the `atomic-cloud` binary (multi-tenant server + operator CLI)
#   - the account-plane SPA        → /srv/spa      (ATOMIC_CLOUD_SPA_DIR)
#   - the product knowledge-base app → /srv/product (ATOMIC_CLOUD_PRODUCT_DIR;
#     the server rewrites its tenant marker to `true` at boot — DEPLOY.md §3)
#   - postgresql-client, so the nightly backups and account-deletion final
#     dumps (`pg_dump`/`pg_restore`) work
#
# Build from the repo root:
#   docker build -f cloud.dockerfile -t atomic-cloud .
#
# The client tools must be at least as new as the tenant cluster —
# `pg_dump` refuses to dump from a newer server. Match your cluster's major:
#   docker build -f cloud.dockerfile --build-arg PG_CLIENT_MAJOR=16 .
#
# All runtime configuration is env-only (`ATOMIC_CLOUD_*`); see DEPLOY.md §7
# for the full checklist. Secrets are read from the environment, never argv.
# =============================================================================

# =============================================================================
# Stage 1a: Cargo Chef planner (dependency caching)
# =============================================================================
FROM rust:1.94-bookworm AS planner
RUN cargo install cargo-chef
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# =============================================================================
# Stage 1b: Cargo Chef cook + build atomic-cloud
# =============================================================================
FROM rust:1.94-bookworm AS rust-builder

# Install mold linker + cargo-chef
RUN apt-get update && apt-get install -y --no-install-recommends mold && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef
WORKDIR /app

# Copy linker config
COPY .cargo/ .cargo/

# Cook dependencies (cached until Cargo.toml/lock changes)
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo chef cook --profile server --recipe-path recipe.json -p atomic-cloud

# Copy real workspace source
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# Workspace stubs for crates we don't build but Cargo needs for resolution
COPY src-tauri/Cargo.toml src-tauri/Cargo.toml
RUN mkdir -p src-tauri/src && \
    echo "fn main() {}" > src-tauri/src/main.rs && \
    echo "pub fn lib() {}" > src-tauri/src/lib.rs && \
    echo "fn main() { tauri_build::build(); }" > src-tauri/build.rs

# Build atomic-cloud with the faster server profile
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --profile server -p atomic-cloud && \
    cp /app/target/server/atomic-cloud /usr/local/bin/atomic-cloud

# =============================================================================
# Stage 2: product knowledge-base app (the tenant-root web bundle)
# =============================================================================
FROM node:24-bookworm-slim AS product-builder
WORKDIR /app

# --ignore-scripts skips better-sqlite3's native compile (a dev-only dep used
# by local db scripts, not needed for `vite build`).
COPY package.json package-lock.json ./
RUN npm ci --ignore-scripts

COPY index.html tsconfig.json tsconfig.node.json vite.config.ts ./
COPY src/ src/
COPY public/ public/
RUN VITE_BUILD_TARGET=web npm run build:web

# The cloud server rewrites this placeholder to `true` at boot; if it ever
# disappears from the bundle, tenant auth silently breaks (DEPLOY.md §3).
# Fail the image build instead.
RUN grep -q '__ATOMIC_CLOUD_TENANT__' dist-web/index.html

# =============================================================================
# Stage 3: account-plane SPA (signup/login + the /account/* dashboard)
# =============================================================================
FROM node:24-bookworm-slim AS spa-builder
WORKDIR /app

COPY crates/atomic-cloud/frontend/package.json crates/atomic-cloud/frontend/package-lock.json ./
RUN npm ci

COPY crates/atomic-cloud/frontend/ ./
RUN npm run build

# =============================================================================
# Runtime
# =============================================================================
FROM debian:bookworm-slim

# pg_dump/pg_restore must be >= the tenant cluster's major (DEPLOY.md §6).
ARG PG_CLIENT_MAJOR=17

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl gnupg && \
    curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
      | gpg --dearmor -o /usr/share/keyrings/pgdg.gpg && \
    echo "deb [signed-by=/usr/share/keyrings/pgdg.gpg] http://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" \
      > /etc/apt/sources.list.d/pgdg.list && \
    apt-get update && \
    apt-get install -y --no-install-recommends postgresql-client-${PG_CLIENT_MAJOR} && \
    apt-get purge -y --auto-remove gnupg && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --shell /bin/false atomic

COPY --from=rust-builder /usr/local/bin/atomic-cloud /usr/local/bin/atomic-cloud
COPY --from=spa-builder /app/dist/ /srv/spa/
COPY --from=product-builder /app/dist-web/ /srv/product/

# Point the server at the baked bundles; both are overridable at runtime.
ENV ATOMIC_CLOUD_SPA_DIR=/srv/spa \
    ATOMIC_CLOUD_PRODUCT_DIR=/srv/product

USER atomic
EXPOSE 8080

# `serve` connects to (creating if absent) and migrates the control plane at
# boot, so no separate `migrate` step is required — but the operator CLI is
# the same binary: `docker run … atomic-cloud migrate|account|token|backup`.
ENTRYPOINT ["atomic-cloud"]
CMD ["serve", "--bind", "0.0.0.0", "--port", "8080"]

HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1
