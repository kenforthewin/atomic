#!/usr/bin/env bash
# Deploy (or update) the Atomic Cloud stack on the droplet.
#
#   deploy/scripts/deploy.sh root@<ip> [--build]
#
# - Syncs deploy/ (including your local .env) to /opt/atomic/deploy
# - Pulls the pod image from GHCR (or --build: clones the repo on the box and
#   builds cloud.dockerfile there — first build takes a while on 4 vCPU)
# - Brings the stack up and verifies /health, /ready, and the §9 boot-warning
#   sweep before declaring success.
set -euo pipefail

HOST=${1:?usage: deploy.sh root@<ip> [--build]}
MODE=${2:-pull}
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DEPLOY_DIR=$(dirname "$SCRIPT_DIR")
REPO_REF=${REPO_REF:-main}
REPO_URL=${REPO_URL:-https://github.com/kenforthewin/atomic.git}

log() { printf '\033[1;36m[deploy]\033[0m %s\n' "$*"; }

[ -f "$DEPLOY_DIR/.env" ] || { echo "deploy/.env missing — copy .env.example and fill it in" >&2; exit 1; }

# shellcheck disable=SC1091
POSTGRES_PASSWORD=$(grep -E '^POSTGRES_PASSWORD=' "$DEPLOY_DIR/.env" | cut -d= -f2-)
BASE_DOMAIN=$(grep -E '^ATOMIC_CLOUD_BASE_DOMAIN=' "$DEPLOY_DIR/.env" | cut -d= -f2-)
[ -n "$POSTGRES_PASSWORD" ] || { echo "POSTGRES_PASSWORD is empty in deploy/.env" >&2; exit 1; }
[ -n "$BASE_DOMAIN" ] || { echo "ATOMIC_CLOUD_BASE_DOMAIN is empty in deploy/.env" >&2; exit 1; }

log "waiting for SSH on ${HOST#*@}"
for _ in $(seq 1 30); do ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new "$HOST" true 2>/dev/null && break; sleep 5; done
ssh "$HOST" 'command -v docker >/dev/null' || { echo "docker missing on host — is cloud-init finished? (cloud-init status --wait)" >&2; exit 1; }

log "syncing deploy/ -> ${HOST}:/opt/atomic/deploy"
rsync -az --delete --exclude scripts "$DEPLOY_DIR/" "$HOST:/opt/atomic/deploy/"
ssh "$HOST" 'chmod 600 /opt/atomic/deploy/.env'

if [ "$MODE" = "--build" ]; then
  log "building the pod image on the box from ${REPO_URL}@${REPO_REF} (grab a coffee)"
  ssh "$HOST" "set -e
    if [ -d /opt/atomic/src/.git ]; then git -C /opt/atomic/src fetch --depth 1 origin '$REPO_REF' && git -C /opt/atomic/src checkout -f FETCH_HEAD
    else git clone --depth 1 --branch '$REPO_REF' '$REPO_URL' /opt/atomic/src; fi
    docker build -f /opt/atomic/src/cloud.dockerfile -t atomic-cloud:local /opt/atomic/src
    sed -i 's|^ATOMIC_CLOUD_IMAGE=.*|ATOMIC_CLOUD_IMAGE=atomic-cloud:local|' /opt/atomic/deploy/.env"
else
  log "pulling the pod image"
  ssh "$HOST" 'cd /opt/atomic/deploy && docker compose pull atomic-cloud'
fi

log "starting the stack"
ssh "$HOST" 'cd /opt/atomic/deploy && docker compose up -d --build caddy && docker compose up -d'

log "waiting for /health"
for i in $(seq 1 60); do
  if ssh "$HOST" 'docker exec $(docker ps -qf name=atomic-cloud-atomic-cloud) curl -fsS http://localhost:8080/health' >/dev/null 2>&1; then break; fi
  [ "$i" = 60 ] && { echo "pod never became healthy; docker compose logs atomic-cloud" >&2; exit 1; }
  sleep 5
done

log "boot-warning sweep (§9: anything below other than email-mode noise is a checklist miss)"
ssh "$HOST" 'cd /opt/atomic/deploy && docker compose logs atomic-cloud 2>&1 | grep -i "WARN" | tail -20' || true

log "public checks"
curl -fsS "https://app.${BASE_DOMAIN}/health" && echo " <- https health OK" || log "https not up yet (DNS/ACME may still be propagating) — retry: curl https://app.${BASE_DOMAIN}/health"

log "done. post-deploy runbook: deploy/README.md (marker check, restore drill, master-key custody)"
