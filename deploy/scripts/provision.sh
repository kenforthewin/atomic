#!/usr/bin/env bash
# Provision the Atomic Cloud droplet + volume + firewall on DigitalOcean and
# point Cloudflare DNS at it. Idempotent: safe to re-run; existing resources
# (matched by name) are reused.
#
# Requires:
#   doctl (authed, or DIGITALOCEAN_ACCESS_TOKEN set)
#   CLOUDFLARE_API_TOKEN   Zone → DNS → Edit on the base domain's zone
#   jq, curl
#
# Tunables (env):
#   DOMAIN       base domain            (default: atomicapp.ai)
#   REGION       DO region              (default: nyc3)
#   SIZE         droplet size           (default: s-4vcpu-8gb)
#   VOLUME_SIZE  Postgres volume        (default: 50GiB)
#   SSH_KEY_ID   DO SSH key id/fingerprint; defaults to every key on the account
set -euo pipefail

DOMAIN=${DOMAIN:-atomicapp.ai}
REGION=${REGION:-nyc3}
SIZE=${SIZE:-s-4vcpu-8gb}
VOLUME_SIZE=${VOLUME_SIZE:-50GiB}
DROPLET_NAME=${DROPLET_NAME:-atomic-cloud}
VOLUME_NAME=atomic-data # cloud-init mounts by this name — do not change
FIREWALL_NAME=${FIREWALL_NAME:-atomic-cloud}
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

: "${CLOUDFLARE_API_TOKEN:?export CLOUDFLARE_API_TOKEN (Zone DNS Edit)}"
command -v doctl >/dev/null || { echo "doctl is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

log() { printf '\033[1;35m[provision]\033[0m %s\n' "$*"; }

# ── Volume (holds the Postgres data dir; encrypted at rest by DO) ───────────
volume_id=$(doctl compute volume list --format ID,Name --no-header | awk -v n="$VOLUME_NAME" '$2==n {print $1}')
if [ -z "$volume_id" ]; then
  log "creating ${VOLUME_SIZE} volume ${VOLUME_NAME} in ${REGION}"
  volume_id=$(doctl compute volume create "$VOLUME_NAME" --region "$REGION" --size "$VOLUME_SIZE" --format ID --no-header)
else
  log "volume ${VOLUME_NAME} exists (${volume_id})"
fi

# ── Droplet ──────────────────────────────────────────────────────────────────
droplet_id=$(doctl compute droplet list --format ID,Name --no-header | awk -v n="$DROPLET_NAME" '$2==n {print $1}')
if [ -z "$droplet_id" ]; then
  if [ -z "${SSH_KEY_ID:-}" ]; then
    SSH_KEY_ID=$(doctl compute ssh-key list --format ID --no-header | paste -sd, -)
    [ -n "$SSH_KEY_ID" ] || { echo "no SSH keys on the DO account; add one or set SSH_KEY_ID" >&2; exit 1; }
  fi
  log "creating droplet ${DROPLET_NAME} (${SIZE}, ${REGION}, Ubuntu 24.04)"
  droplet_id=$(doctl compute droplet create "$DROPLET_NAME" \
    --region "$REGION" --size "$SIZE" --image ubuntu-24-04-x64 \
    --ssh-keys "$SSH_KEY_ID" \
    --volumes "$volume_id" \
    --user-data-file "$SCRIPT_DIR/../cloud-init.yml" \
    --tag-name atomic-cloud \
    --format ID --no-header --wait)
else
  log "droplet ${DROPLET_NAME} exists (${droplet_id})"
fi

ip=$(doctl compute droplet get "$droplet_id" --format PublicIPv4 --no-header)
log "droplet IP: ${ip}"

# ── Firewall: 22/80/443 in, everything out ──────────────────────────────────
fw_id=$(doctl compute firewall list --format ID,Name --no-header | awk -v n="$FIREWALL_NAME" '$2==n {print $1}')
if [ -z "$fw_id" ]; then
  log "creating firewall ${FIREWALL_NAME}"
  doctl compute firewall create --name "$FIREWALL_NAME" \
    --inbound-rules "protocol:tcp,ports:22,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:80,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:443,address:0.0.0.0/0,address:::/0 protocol:udp,ports:443,address:0.0.0.0/0,address:::/0" \
    --outbound-rules "protocol:tcp,ports:0,address:0.0.0.0/0,address:::/0 protocol:udp,ports:0,address:0.0.0.0/0,address:::/0 protocol:icmp,address:0.0.0.0/0,address:::/0" \
    --droplet-ids "$droplet_id" >/dev/null
else
  doctl compute firewall add-droplets "$fw_id" --droplet-ids "$droplet_id" 2>/dev/null || true
  log "firewall ${FIREWALL_NAME} exists; droplet attached"
fi

# ── Cloudflare DNS: apex + wildcard A records (DNS-only / grey cloud) ───────
cf() { curl -fsS -H "Authorization: Bearer $CLOUDFLARE_API_TOKEN" -H 'Content-Type: application/json' "$@"; }
zone_id=$(cf "https://api.cloudflare.com/client/v4/zones?name=${DOMAIN}" | jq -r '.result[0].id')
[ "$zone_id" != null ] || { echo "zone ${DOMAIN} not found on this Cloudflare account/token" >&2; exit 1; }

upsert_record() { # name
  local name=$1 rec_id
  rec_id=$(cf "https://api.cloudflare.com/client/v4/zones/${zone_id}/dns_records?type=A&name=${name}" | jq -r '.result[0].id // empty')
  local body
  body=$(jq -nc --arg name "$name" --arg ip "$ip" '{type:"A",name:$name,content:$ip,ttl:300,proxied:false}')
  if [ -n "$rec_id" ]; then
    cf -X PUT "https://api.cloudflare.com/client/v4/zones/${zone_id}/dns_records/${rec_id}" --data "$body" >/dev/null
    log "updated A ${name} -> ${ip}"
  else
    cf -X POST "https://api.cloudflare.com/client/v4/zones/${zone_id}/dns_records" --data "$body" >/dev/null
    log "created A ${name} -> ${ip}"
  fi
}
upsert_record "$DOMAIN"       # apex (app plane; app.<domain> rides the wildcard)
upsert_record "*.${DOMAIN}"   # every tenant subdomain + app.<domain>

log "done. next: fill deploy/.env (see .env.example), then:"
log "  deploy/scripts/deploy.sh root@${ip}"
