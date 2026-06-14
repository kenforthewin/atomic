#!/usr/bin/env bash
#
# cloud-dev.sh — bring up the whole Atomic Cloud dev stack in one command.
#
# Hybrid model: Postgres runs in Docker (docker-compose.test.yml); the cloud
# server runs on the HOST via `cargo run` (fast incremental rebuilds, no heavy
# in-container Rust build). It serves the account-plane frontend AND the
# product knowledge-base app on one origin, so the dashboard's "Open knowledge
# base" link works end to end.
#
# Usage:
#   scripts/cloud-dev.sh              # build frontends (if needed) + serve
#   SKIP_BUILD=1 scripts/cloud-dev.sh # skip the frontend builds (fast restart)
#   REBUILD=1 scripts/cloud-dev.sh    # force-rebuild both frontends
#
# Then, on the machine with your browser (see crates/atomic-cloud/TESTING.md
# for the Tailscale/SSH setup): resolve *.cloudtest.local to this box
# (dnsmasq wildcard or /etc/hosts) and open http://app.cloudtest.local:8080/.
#
set -euo pipefail

cd "$(dirname "$0")/.."
REPO="$(pwd)"

# --- config (override via env) ---------------------------------------------
PG_PORT="${PG_PORT:-5433}"
PORT="${PORT:-8080}"
BIND="${BIND:-127.0.0.1}"                 # 0.0.0.0 to reach it directly over Tailscale
BASE_DOMAIN="${BASE_DOMAIN:-cloudtest.local}"
APP_PUBLIC_URL="${APP_PUBLIC_URL:-http://app.${BASE_DOMAIN}:${PORT}}"
PGURL="postgres://atomic:atomic_test@localhost:${PG_PORT}"
CONTROL_URL="${CONTROL_URL:-${PGURL}/atomic_cloud_dev}"   # a DEV control DB, never the test suite's
CLUSTER_URL="${CLUSTER_URL:-${PGURL}/atomic_test}"
ACCOUNT_DIST="${ACCOUNT_DIST:-crates/atomic-cloud/frontend/dist}"
PRODUCT_DIST="${PRODUCT_DIST:-dist-web}"

# A stable dev master key, persisted so restarts reuse it (provider creds stay
# decryptable across runs). Gitignored.
KEY_FILE="${REPO}/.cloud-dev-master-key"
if [[ ! -f "$KEY_FILE" ]]; then
  openssl rand -hex 32 > "$KEY_FILE"
  echo "cloud-dev: generated a dev master key at .cloud-dev-master-key"
fi
export ATOMIC_CLOUD_MASTER_KEY="$(cat "$KEY_FILE")"

say() { printf '\n\033[1;35mcloud-dev:\033[0m %s\n' "$*"; }

# --- 1. Postgres ------------------------------------------------------------
say "ensuring Postgres is up (docker compose, port ${PG_PORT})"
docker compose -f docker-compose.test.yml up -d
say "waiting for Postgres to accept connections"
for i in $(seq 1 30); do
  if pg_isready -h localhost -p "${PG_PORT}" -U atomic >/dev/null 2>&1; then break; fi
  if [[ $i -eq 30 ]]; then echo "Postgres did not come up on :${PG_PORT}" >&2; exit 1; fi
  sleep 1
done

# --- 2. frontends -----------------------------------------------------------
build_account() {
  say "building the account-plane frontend → ${ACCOUNT_DIST}"
  ( cd crates/atomic-cloud/frontend && { [[ -d node_modules ]] || npm ci; } && npm run build )
}
build_product() {
  say "building the product web app → ${PRODUCT_DIST}"
  ( [[ -d node_modules ]] || npm ci; npm run build:web )
}
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  if [[ "${REBUILD:-0}" == "1" || ! -f "${ACCOUNT_DIST}/index.html" ]]; then build_account; fi
  if [[ "${REBUILD:-0}" == "1" || ! -f "${PRODUCT_DIST}/index.html" ]]; then build_product; fi
else
  say "SKIP_BUILD=1 — using existing frontend builds"
fi

# --- 3. migrate the control plane ------------------------------------------
say "migrating the control plane (${CONTROL_URL##*/})"
cargo run -q -p atomic-cloud -- --control-url "${CONTROL_URL}" migrate

# --- 4. serve ---------------------------------------------------------------
say "starting the cloud server on http://${BIND}:${PORT}  (Ctrl-C to stop)"
say "open http://app.${BASE_DOMAIN}:${PORT}/  — magic-link login URLs print below"
echo
exec cargo run -q -p atomic-cloud -- --control-url "${CONTROL_URL}" serve \
  --cluster-url "${CLUSTER_URL}" \
  --base-domain "${BASE_DOMAIN}" \
  --app-public-url "${APP_PUBLIC_URL}" \
  --bind "${BIND}" --port "${PORT}" \
  --email-mode log \
  --dangerously-insecure-cookies \
  --reaper-interval-secs 0 \
  --spa-dir "${ACCOUNT_DIST}" \
  --product-dir "${PRODUCT_DIST}"
