# Atomic Cloud — production deploy (DigitalOcean single box)

Automation for the topology in [`crates/atomic-cloud/DEPLOY.md`](../crates/atomic-cloud/DEPLOY.md):
one droplet running the pod image behind Caddy (wildcard TLS via Cloudflare
DNS-01), a colocated pgvector Postgres on an encrypted DO volume, backups to
Cloudflare R2. DEPLOY.md stays the source of truth for *why*; this directory
is the *how*.

The app connects to Postgres **directly** — no pgbouncer. Transaction pooling
breaks the app's session advisory locks (verified; DEPLOY.md §6), and at
single-box scale the budget is `max_connections=200` + the app's pool caps.

DNS: only the **wildcard** `*.atomicapp.ai` points at the droplet (it covers
`app.` and every tenant). The **apex stays on the marketing site** — served
through Cloudflare, untouched by provision.sh unless `APEX_DNS=1`.

```
deploy/
  docker-compose.yml   caddy + postgres + atomic-cloud
  Caddyfile            host passthrough, wildcard cert, X-Forwarded-For
  caddy.dockerfile     caddy + cloudflare DNS plugin (DNS-01)
  cloud-init.yml       droplet first boot: docker, swap, volume mount
  .env.example         every knob, mapped to DEPLOY.md §8 — copy to .env
  scripts/provision.sh droplet + volume + firewall + Cloudflare DNS (idempotent)
  scripts/deploy.sh    sync, image pull/build, compose up, §9 verification
```

## Credentials needed

| Credential | Scope | Used by |
|---|---|---|
| DigitalOcean API token | write | `provision.sh` (doctl) |
| Cloudflare API token | Zone → DNS → Edit on the base domain | `provision.sh` (records), Caddy (DNS-01), runtime `.env` |
| R2 S3 credentials + endpoint | Object read/write on the backup bucket | runtime `.env` |
| OpenRouter provisioning key | managed per-account keys | runtime `.env` |
| Mailgun API key + domain | magic-link email | runtime `.env` |
| Stripe keys + price map | optional — omit to launch billing-disabled | runtime `.env` |

## Initial deploy

```bash
# 0. one-time local prereqs: doctl (authed), jq, rsync, an SSH key on the DO account
export CLOUDFLARE_API_TOKEN=...   # Zone DNS Edit

# 1. infrastructure (idempotent; prints the droplet IP)
deploy/scripts/provision.sh

# 2. configuration
cp deploy/.env.example deploy/.env && $EDITOR deploy/.env
#    openssl rand -hex 32  -> ATOMIC_CLOUD_MASTER_KEY  (custody: see below)
#    openssl rand -hex 24  -> POSTGRES_PASSWORD

# 3. the stack (use --build until the GHCR image exists/is public)
deploy/scripts/deploy.sh root@<ip> --build
```

The pod image normally comes from GHCR, published by
`.github/workflows/cloud-image.yml` (dispatch it manually for a branch build).
Make the `atomic-cloud` package public once after its first push, or the
droplet can't pull anonymously — until then `--build` compiles on the box.

## Post-deploy verification (do not skip)

1. `deploy.sh` already checks `/health`, `/ready`, and prints the boot-warning
   sweep — the §9 rule: any warning besides email-mode noise is a checklist miss.
2. Marker check after signup (§3): `curl -s https://<slug>.<base>/ | grep atomic-cloud-tenant`
   must show `content="true"`.
3. **Master-key custody** (§2): `ATOMIC_CLOUD_MASTER_KEY` exists in exactly two
   places — `deploy/.env` on the box and your password manager. Never in the
   R2 bucket, never in git.
4. **Restore drill** (DEPLOY.md backups runbook): create an account, run
   `docker compose exec atomic-cloud atomic-cloud backup run`, restore the dump
   into a fresh DB, repoint, verify. Rehearse before real users exist.

## Updates

```bash
deploy/scripts/deploy.sh root@<ip>          # pull latest image, compose up -d
deploy/scripts/deploy.sh root@<ip> --build  # or build a branch: REPO_REF=<ref>
```

A deploy is a brief blip (single-pod v1, §5). `/ready` gates on boot fleet
migration; `docker compose exec atomic-cloud atomic-cloud deploy status`
inspects it.

## Known-expected warnings

- `--control-url/--cluster-url does not require TLS`: expected — the app↔PG
  link is a docker bridge on one host (§2's out-of-band case).
- `email mode is 'log'`: only acceptable before Mailgun is wired, never with
  real users.
