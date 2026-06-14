# Running Atomic Cloud locally (headless box, SSH over Tailscale)

A practical guide to standing up the whole cloud stack — account-plane frontend
+ APIs + a tenant — on a **headless dev box** you reach over **Tailscale via
SSH**, so you can click through signup, the dashboard, billing, and MCP in a
real browser.

The cloud frontend routes by `Host` (`app.<base>` = signup/login,
`<slug>.<base>` = the tenant dashboard), so the one wrinkle a headless setup
adds is getting those hostnames to resolve to the box and getting the session
cookie to work over plain HTTP. Both are handled below.

---

## TL;DR

On the **box** (over SSH):

```bash
cd ~/git/atomic
docker compose -f docker-compose.test.yml up -d                 # Postgres on :5433

npm --prefix crates/atomic-cloud/frontend ci
npm --prefix crates/atomic-cloud/frontend run build             # produces frontend/dist

export ATOMIC_CLOUD_MASTER_KEY=$(openssl rand -hex 32)
CTL=postgres://atomic:atomic_test@localhost:5433/atomic_cloud_dev   # NB: a DEV control DB, not the test one
CLUSTER=postgres://atomic:atomic_test@localhost:5433/atomic_test

cargo run -p atomic-cloud -- --control-url "$CTL" migrate
cargo run -p atomic-cloud -- --control-url "$CTL" serve \
  --cluster-url "$CLUSTER" \
  --base-domain cloudtest.local \
  --app-public-url http://app.cloudtest.local:8080 \
  --bind 127.0.0.1 --port 8080 \
  --email-mode log \
  --dangerously-insecure-cookies
```

On your **laptop**:

```bash
ssh -L 8080:127.0.0.1:8080 <box>            # tunnel the cloud port over your Tailscale SSH
# add to /etc/hosts (on the laptop):
#   127.0.0.1  app.cloudtest.local cloudtest.local alpha.cloudtest.local
```

Then browse `http://app.cloudtest.local:8080/`.

---

## Why those flags

The two headless-specific flags matter; the rest are ordinary dev settings.

- **`--dangerously-insecure-cookies`** — the session cookie is normally
  `Secure` (HTTPS-only). Browsers only exempt `localhost`/`*.localhost` from
  that rule, so over plain HTTP on any *other* host (a Tailscale IP, a
  MagicDNS name, or `app.cloudtest.local`) the browser **silently drops the
  cookie** and the dashboard can never log you in. This flag drops `Secure`
  for dev. **Never use it in production** — the server logs a loud warning at
  boot when it's set. (`HttpOnly`, `SameSite=Lax`, and the base-domain scope
  stay on.)
- **`--app-public-url http://app.cloudtest.local:8080`** — the origin used for
  emailed magic links and for the post-login redirect to your tenant
  subdomain. It carries the scheme **and port**, so links land on
  `http://...:8080` instead of a default-port HTTPS URL you can't reach.
- **`--base-domain cloudtest.local`** — any name works as long as it matches
  your `/etc/hosts` entries; `cloudtest.local` is the convention used in the
  e2e tests.
- **`--email-mode log`** — magic links are written to the server log instead
  of emailed. You copy the link from the log (see below).

## Access: SSH tunnel (recommended) vs direct Tailscale

**SSH tunnel (above)** is the simplest and keeps the box's port closed to the
tailnet. Bind `127.0.0.1`, `ssh -L 8080:127.0.0.1:8080 <box>`, and point the
laptop's `/etc/hosts` at `127.0.0.1`. The browser hits `…:8080` → tunnel → box.

**Direct over Tailscale** (e.g. to test from a phone on the tailnet): bind the
Tailscale interface and point `/etc/hosts` at the box's Tailscale IP instead:

```bash
# on the box:
cargo run -p atomic-cloud -- ... --bind 0.0.0.0 --app-public-url http://app.cloudtest.local:8080 ...
# on each client, /etc/hosts:
#   100.x.y.z  app.cloudtest.local cloudtest.local alpha.cloudtest.local   (the box's Tailscale IP)
```

Either way the client `/etc/hosts` is what makes the `Host`-based subdomain
routing resolve — there's no wildcard DNS for `*.cloudtest.local`.

## Create an account and log in

In a second shell **on the box** (same `CTL`/`CLUSTER` env):

```bash
cargo run -p atomic-cloud -- --control-url "$CTL" account create \
  --cluster-url "$CLUSTER" --email you@example.com --subdomain alpha
```

That provisions the `alpha` tenant and prints a one-time account token (handy
for `curl`). For the **browser** dashboard you need a session cookie, which you
get from the magic-link flow:

1. Browse `http://app.cloudtest.local:8080/login`, enter the account email.
2. The server log prints a line with a `…/login/complete?token=aml_…` URL.
   Open that URL in the browser — it sets the session cookie and redirects you
   to `http://alpha.cloudtest.local:8080/`.
3. Visit `http://alpha.cloudtest.local:8080/account` — the dashboard
   (overview, provider, billing, MCP, danger zone).

> New signups work the same way: `/signup` → grab the `…/signup/complete?token=…`
> link from the log → it provisions a fresh tenant and logs you in.

## What to click through

- **`app.cloudtest.local:8080/`** — landing, `/signup` (live `<slug>.<base>`
  preview, validation + rate-limit handling), `/login` (existence-neutral).
- **`alpha.cloudtest.local:8080/account`** — Overview (plan/usage/banners),
  Provider/BYOK settings, Billing (the "Manage billing" button needs Stripe
  keys; without them it shows the configured-off state), MCP setup, and the
  typed-confirmation delete flow.

## Fast visual iteration (Vite HMR)

For tight styling loops, run the Vite dev server (hot reload) instead of
rebuilding:

```bash
# on the box:  npm --prefix crates/atomic-cloud/frontend run dev   (Vite on :5173, proxies /api to :8080)
# on the laptop:  ssh -L 5173:127.0.0.1:5173 -L 8080:127.0.0.1:8080 <box>
```

Keep the cloud server (`serve`) running for the APIs. HMR is great for
component/styling tweaks; the full `Host`-based routing + cookie auth is most
faithful through the built-and-served path above, so do a final pass there.

## Two things that will bite you

1. **Never point a dev `serve` at the test cluster (`atomic_cloud_test_*` / the
   suite's DBs) while tests run.** The server's reaper drops tenant databases
   it doesn't recognize, which includes the test suite's. Use a **separate dev
   control DB** (`atomic_cloud_dev`, as above) and **stop the dev server before
   running `cargo test`**. The tenant data cluster (`atomic_test`) is shared,
   but the reaper keys off the control DB, so a distinct control DB keeps them
   apart.
2. **`--dangerously-insecure-cookies` is dev-only.** It exists solely so the
   session cookie survives plain HTTP on a non-`localhost` host. Production
   runs over HTTPS without it.

## Teardown

```bash
# stop the dev server (Ctrl-C), then optionally drop the dev databases:
PGPASSWORD=atomic_test psql -h localhost -p 5433 -U atomic -d atomic_test \
  -c "DROP DATABASE IF EXISTS atomic_cloud_dev WITH (FORCE);"
# tenant DBs (acct_*) created by your dev account can be dropped the same way,
# or via:  atomic-cloud --control-url "$CTL" account delete --subdomain alpha
```
