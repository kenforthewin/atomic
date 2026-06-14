#!/usr/bin/env bash
#
# cloud-reset.sh — wipe the Atomic Cloud dev stack back to an empty slate.
#
# Drops the Postgres Docker volume entirely, so EVERY tenant database and the
# dev control plane go away in one shot, then brings Postgres back up and
# re-initialises the control plane. The next `scripts/cloud-dev.sh` starts with
# zero accounts.
#
# Stop a running `scripts/cloud-dev.sh` first — this pulls Postgres out from
# under it.
#
# Heads up: if you tested MANAGED OpenRouter provisioning, this erases the
# stored credential rows WITHOUT revoking the real per-tenant keys on
# OpenRouter (they live there, not here). Revoke any stragglers in the
# OpenRouter provisioning dashboard.
#
# Usage:
#   scripts/cloud-reset.sh            # confirm, then wipe + re-init
#   FORCE=1 scripts/cloud-reset.sh    # skip the confirmation prompt
set -euo pipefail

cd "$(dirname "$0")/.."

# --- config (override via env; mirrors cloud-dev.sh) -----------------------
PG_PORT="${PG_PORT:-5433}"
PGURL="postgres://atomic:atomic_test@localhost:${PG_PORT}"
CONTROL_URL="${CONTROL_URL:-${PGURL}/atomic_cloud_dev}"

say() { printf '\n\033[1;35mcloud-reset:\033[0m %s\n' "$*"; }

# --- confirm ----------------------------------------------------------------
if [[ "${FORCE:-0}" != "1" ]]; then
  printf '\033[1;31mThis destroys ALL cloud dev data (every tenant + the control plane).\033[0m\n'
  read -r -p "Wipe the Postgres volume and re-initialise? [y/N] " reply
  [[ "$reply" =~ ^[Yy]$ ]] || { echo "aborted."; exit 1; }
fi

# --- 1. drop the volume -----------------------------------------------------
# `down -v` removes the container and its (anonymous) data volume — a far
# cleaner reset than dropping databases one by one.
say "tearing down Postgres and its data volume"
docker compose -f docker-compose.test.yml down -v

# --- 2. bring it back -------------------------------------------------------
say "starting a fresh Postgres (port ${PG_PORT})"
docker compose -f docker-compose.test.yml up -d
say "waiting for Postgres to accept connections"
for i in $(seq 1 30); do
  if pg_isready -h localhost -p "${PG_PORT}" -U atomic >/dev/null 2>&1; then break; fi
  if [[ $i -eq 30 ]]; then echo "Postgres did not come up on :${PG_PORT}" >&2; exit 1; fi
  sleep 1
done

# --- 3. re-initialise the control plane -------------------------------------
# `migrate` creates the control database if absent (ControlPlane::connect →
# ensure_database_exists) and applies every migration, so the stack is ready
# without a first serve.
say "initialising the control plane (${CONTROL_URL##*/})"
cargo run -q -p atomic-cloud -- --control-url "${CONTROL_URL}" migrate

say "clean slate — start the stack with: scripts/cloud-dev.sh"
