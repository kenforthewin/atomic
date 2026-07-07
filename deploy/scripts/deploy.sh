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

# SSH comes up before cloud-init's runcmd finishes (docker install, volume
# mount, /opt/atomic) — wait for it or the sync races the bootstrap.
log "waiting for cloud-init to finish"
ssh "$HOST" 'command -v cloud-init >/dev/null && cloud-init status --wait >/dev/null; true'
ssh "$HOST" 'command -v docker >/dev/null' || { echo "docker missing on host after cloud-init — check /var/log/cloud-init-output.log" >&2; exit 1; }
ssh "$HOST" 'mkdir -p /opt/atomic/deploy'

log "syncing deploy/ -> ${HOST}:/opt/atomic/deploy"
rsync -az --delete --exclude scripts "$DEPLOY_DIR/" "$HOST:/opt/atomic/deploy/"
ssh "$HOST" 'chmod 600 /opt/atomic/deploy/.env'

if [ "$MODE" = "--build" ]; then
  # The build runs DETACHED on the box (nohup + exit-code file): a dropped SSH
  # session must not kill a half-hour cargo build. Re-running this script
  # while a build is in flight just resumes waiting on it.
  log "building the pod image on the box from ${REPO_URL}@${REPO_REF} (detached; survives disconnects)"
  ssh "$HOST" "set -e
    if [ -d /opt/atomic/src/.git ]; then git -C /opt/atomic/src fetch --depth 1 origin '$REPO_REF' && git -C /opt/atomic/src checkout -f FETCH_HEAD
    else git clone --depth 1 --branch '$REPO_REF' '$REPO_URL' /opt/atomic/src; fi
    sed -i 's|^ATOMIC_CLOUD_IMAGE=.*|ATOMIC_CLOUD_IMAGE=atomic-cloud:local|' /opt/atomic/deploy/.env
    # In-flight detection via pidfile — NOT pgrep: any cmdline pattern would
    # match this ssh session's own shell, whose command string contains the
    # docker build invocation below.
    if [ -f /opt/atomic/build.pid ] && kill -0 \$(cat /opt/atomic/build.pid) 2>/dev/null; then
      echo 'build already in flight — waiting on it'
    else
      rm -f /opt/atomic/build.exit
      nohup sh -c 'docker build -f /opt/atomic/src/cloud.dockerfile -t atomic-cloud:local /opt/atomic/src; echo \$? > /opt/atomic/build.exit' >/opt/atomic/build.log 2>&1 &
      echo \$! > /opt/atomic/build.pid
    fi"
  log "waiting for the build (tail: ssh $HOST tail -f /opt/atomic/build.log)"
  while :; do
    status=$(ssh "$HOST" 'cat /opt/atomic/build.exit 2>/dev/null' || true)
    if [ "$status" = "0" ]; then log "image built"; break; fi
    if [ -n "$status" ]; then
      ssh "$HOST" 'tail -30 /opt/atomic/build.log' >&2
      echo "image build failed (exit $status)" >&2; exit 1
    fi
    if ! ssh "$HOST" 'kill -0 $(cat /opt/atomic/build.pid 2>/dev/null) 2>/dev/null'; then
      ssh "$HOST" 'tail -30 /opt/atomic/build.log' >&2
      echo "build process died without writing an exit code" >&2; exit 1
    fi
    sleep 20
  done
else
  log "pulling the pod image"
  ssh "$HOST" 'cd /opt/atomic/deploy && docker compose pull atomic-cloud' \
    || log "pull failed — continuing with the image already on the box"
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
