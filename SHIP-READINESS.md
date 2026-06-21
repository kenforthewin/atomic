# Atomic Cloud — Ship-Readiness Assessment

**Date:** 2026-06-21
**Branch under review:** `cloud-frontend`
**Scope:** Atomic Cloud multi-tenant SaaS (control plane, tenant provisioning, billing, isolation, background fan-out, frontend, ops/security)
**Method:** static analysis — 14 parallel domain deep-dives + adversarial verification of every blocker/major finding (assess-only)

---

## Executive Summary

**Verdict: CONDITIONAL-GO.** One true blocker must be fixed before public launch: the per-tenant **MCP surface bypasses every data-plane billing/quota/write-block/rate-limit guard** (BILL-1). It is a metering escape and a non-payment write-block escape that ships open, reachable by any authenticated tenant with an ordinary token. Everything else is a major/fast-follow that does not, on its own, fully break a core flow.

The isolation architecture is genuinely strong and was the most heavily scrutinized area — cross-tenant data, credential, and SQL-injection chokepoints all hold (ISO-1..5, AUTH-3..4 chokepoints, SEC crypto). No cross-tenant data breach was found. The one false-positive (AUTH-2, broad-domain cookie) was correctly dismissed as unreachable defense-in-depth.

The three most important takeaways for the operator:

1. **Fix BILL-1 before launch.** Wrap `mcp_scope` with the data-plane guards (or gate atom creation against the resolved tenant's billing/storage state). It is the single thing that turns a "go" into a "no-go" for the Billing bucket.
2. **Two operator footguns silently void data-safety guarantees** (MIG-1 local backup store, REL-4 orphan-reaper) and must be neutralized with boot warnings/guards before any production pod is stood up — they are not tenant-triggerable but are catastrophic if an operator missteps, and there is no production runbook (OPS-1/OPS-2) to prevent the misstep.
3. **The paid-AI experience is broken by default** (MAI-1/BILL-2): managed keys are hard-capped at the fleet-wide $0.50/mo regardless of plan, and the default trial puts every signup on the "pro" tier with a $0.50 key. If any paid/trial AI tier is live at launch, treat MAI-1 as a same-day fast-follow.

**1 blocker dropped** as a false positive: AUTH-2 (cross-tenant cookie harvest) — unreachable today behind HttpOnly + the no-tenant-active-content invariant; retained in the risk register as an accepted hardening item. Two findings were severity-adjusted down by their verdicts: BILL-2 (major→minor) and OPS-3 (major→minor).

---

## Ship Blockers

### BILL-1 — MCP `/mcp` surface bypasses every data-plane guard (quota, write-block, rate-limit, out-of-credits)

- **Severity:** blocker (3/3 adversarial votes: blocker)
- **Dimension:** billing / metering integrity
- **Evidence:** `crates/atomic-cloud/src/server.rs:482-486` registers `mcp_scope` wrapped with ONLY `cloud_plane_guard` + `CloudAuth`; the six data-plane guards (`mark_hint_on_mutation`, `chat_stream_guard`, `out_of_credits_guard`, `quota_guard`, `billing_write_guard`, `data_plane_rate_limit_guard`) are wrapped exclusively on `api_scope` at `server.rs:518-525` and never touch `mcp_scope`. The MCP `create_atom` tool (`crates/atomic-server/src/mcp/server.rs:207-229`) calls `core.create_atom(...)` directly, firing the embedding/tagging/edge pipeline. `quota.rs:133-163` only path-matches REST routes and returns `None` for `/mcp` (moot, since the guard isn't on the scope at all). Reachable by ANY valid token scope (Account/Database/Mcp) — CloudAuth enforces only `allowed_db_id`, never `TokenScope` per-path (`auth.rs:437`); MCP-scope tokens are mintable via the public per-account OAuth flow.
- **Impact:** A tenant can, via `/mcp create_atom`: (1) create atoms past the plan `atom_limit` (free tier's 100-atom ceiling unenforced) — a direct metering escape; (2) keep writing while the account is dunning `read_only` or storage `restricted` — the user-visible non-payment enforcement is defeated; (3) drive AI-pipeline work while `out_of_ai_credits`; (4) evade the 600 req/min + 60 creates/min anti-abuse limiter. Suspended accounts ARE still blocked (CloudAuth `blocks_serving` wraps the scope), and OpenRouter's per-key cap bounds managed *spend* — so this is not unbounded-cost, but the resource ceiling and the non-payment write-block ship open.
- **Recommendation:** Wrap `mcp_scope` with `billing_write_guard`, `quota_guard`, `data_plane_rate_limit_guard`, and `out_of_credits_guard` (teaching `quota_target` to recognize the MCP create-atom JSON-RPC call), OR gate atom creation inside the cloud's per-request MCP resolution against the resolved tenant's billing/storage state and live atom count. At minimum the `read_only`/`restricted` write-block MUST hold for MCP — it is `billing_write_guard`'s method-based check (POST mutates, MCP JSON-RPC is POST), so it would fire if simply present.
- **Confidence:** high

---

## Major / Fast-Follow

These are real and reachable but do NOT block a soft launch under the tiering rule (reliability/UX/ops do not block unless they fully break a core flow). Prioritized roughly by blast radius. **If a paid/trial AI tier is live at launch, MAI-1 and DEL-1 jump to same-day priority.**

### Billing / Cost (fast-follow, but launch-critical if paid tier is live)

- **MAI-1 / BILL-2 (clustered) — Managed-key AI allowance never reconciled with plan; `update_key_limit` is dead code.** *(major; billing)* `managed_keys.rs:191-197` mints the key with the fleet-wide `--managed-key-allowance-cents` (default 50¢), never the plan's `ai_credits_monthly_cents` (free=50, pro=2000; `migration 010:42-47`). `ProvisioningApi::update_key_limit` (`provisioning_api.rs:120,332`) has zero production callers; no plan-transition path (`dunning.rs` `set_plan`/`start_trial`/`finish_expired_trial`) touches the key. **Reachability is worse than stated:** the default trial (`account_plane.rs:296`, plan_id="pro") puts every signup on the pro tier with a $0.50 key, so AI dies after $0.50 of spend on a freshly-signed-up account — no upgrade action needed. Cost-SAFE (cap too low, never too high). **Fix:** call `update_key_limit(external_key_id, plan.ai_credits_monthly_cents)` on every plan transition and provision from the plan, not the fleet flag. (Note: BILL-2's verdict downgraded it to minor on the grounds that `ai_credits_monthly_cents` is advisory and pro pricing is a placeholder; MAI-1 holds it at major because the AI experience genuinely breaks. Treated here as one major cluster.) *Sources: MAI-1, BILL-2.*
- **DEL-1 — Account deletion never cancels the tenant's Stripe subscription.** *(major; billing)* `provision.rs:716-878` performs all 7 destructive steps with NO Stripe interaction; `BillingProvider` (`billing.rs:124-145`) has no `cancel_subscription` method. The `accounts` CASCADE wipes the local `stripe_subscriptions` row, so the platform also loses its pointer. **Impact:** a paid tenant who deletes their account keeps being billed indefinitely for a destroyed workspace — refund/chargeback/trust exposure. **Fix:** add `BillingProvider::cancel_subscription`, call it best-effort inside `delete_account` before the CASCADE; update Danger-page copy. *Source: DEL-1.*
- **DISP-1 — Dispatcher ignores billing `read_only`/`suspended`/`restricted` holds.** *(major; billing)* `held_accounts()` (`dispatcher.rs:1122-1150`) filters only on `provider_paused_until` and migration lag; delinquent accounts stay `status='active'` and keep draining pipeline/report/feed-poll work (bounded managed-spend leak ≤ allowance/mo + dunning-policy escape). **Fix:** extend `held_accounts` SQL to also hold `billing_state IN ('read_only','suspended')` / `storage_state='restricted'`. *Source: DISP-1.*

### Data-Safety / Ops Footguns (must neutralize before standing up production)

- **MIG-1 — Backup store silently defaults to local ephemeral disk; no boot warning.** *(major; data-safety)* `main.rs:472` defaults `--backup-store local`; the fail-closed "final dump before DROP DATABASE" guard (`provision.rs:792-830`) accepts a local-disk write as success. On a production pod with ephemeral storage, a tenant hard-delete writes its only undo to disk that evaporates on restart. **Fix:** loud boot `warn!` (mirroring `--dangerously-insecure-cookies`) when `--backup-store local`, or gate on non-localhost base domain. *Source: MIG-1.*
- **REL-4 — Orphan-database reaper DROPs any `acct_*` DB the control plane doesn't reference.** *(major; data-safety)* `reaper.rs:853-944` drops unreferenced tenant DBs; the under-lock re-check re-queries the SAME (possibly stale/empty) control plane. A misdirected `--control-url` after failover/restore → fleet-wide irreversible data loss within 60s (default reaper on, no per-pass cap, no final dump on this arm). **Fix:** refuse orphan reclaim when control plane has zero `accounts` but cluster has many `acct_*` DBs; cap drops per pass; alert on implausibly-high orphan counts; stamp tenant DBs with control-plane identity. *Source: REL-4.*
- **OPS-1 / OPS-2 (clustered) — No production deployment runbook; reverse-proxy product-app serving silently breaks tenant auth.** *(major; ops)* The `__ATOMIC_CLOUD_TENANT__` marker is injected ONLY when the cloud server serves the product app via `--product-dir` (`spa.rs:162`); the documented prod topology (reverse proxy serves the bundle) omits injection, so `isCloudTenant()` returns false and every tenant lands on the self-hosted setup screen — core login broken. No `DEPLOY.md` exists (wildcard TLS, host-split routing, `--trust-proxy-header`, single-pod LISTEN/NOTIFY constraint, env checklist all undocumented). **Fix:** write a production runbook; have the proxy/build inject the meta tag (or warn/fail if a tenant-root product response lacks it). *Sources: OPS-1, OPS-2.*

### Security

- **AUTH-1 / DASH-4 (clustered) — No logout / session-revocation endpoint.** *(major; security)* `SESSION_TTL=30d` HttpOnly cookie (`account_plane.rs:194,949-959`); no logout route exists; sessions are structurally non-revocable (`sessions` table has no `revoked_at`; `verify_session` at `tokens.rs:254-269` checks only `account_id+hash+expires_at`). Dashboard "Sign out" is a cosmetic `<a href>`. **Mitigated** by magic-link-only auth (no password to phish), bounded TTL with reaper purge, account-deletion invalidation, and account_id-scoped verify (no cross-tenant reach). **Fix:** add `POST /account/logout` that deletes the session row(s) and returns `Set-Cookie ...Max-Age=0`; wire the button. *Sources: AUTH-1, DASH-4.*
- **SEC-1 — SSRF via unrestricted BYOK base-URL with read-oracle echo.** *(major; security)* `validate_byok_model_config` (`provider_config.rs:143-169`) only checks `is_string()` — no scheme/host allowlist. The URL drives outbound GET/POST at validation and on every live pipeline call; the OpenRouter arm echoes the response body (truncated 500c, key-scrubbed, non-2xx only) back in the 400. Any authenticated tenant can probe internal addresses (169.254.169.254, control-plane Postgres, east-west) on shared infra. **Fix:** require https, reject private/loopback/link-local/metadata hosts, resolve-and-pin against DNS rebinding (or route egress through a deny-list forward proxy); stop echoing raw response bodies. *Source: SEC-1.*

### Reliability

- **REL-1 — Control-plane pool hardcoded to 5 connections, shared by auth path + 8 background loops, no tuning knob.** *(major; reliability)* `control_plane.rs:106-112` (`max_connections(5)`, no env/flag); 112 `control.pool()` call sites; every CloudAuth request does ≥2 sequential control queries with no caching; saturation → 10s acquire timeout → spurious 500s to healthy tenants. **Fix:** add `--control-pool-max-connections` (default 20-30), consider a separate pool for background loops. *Source: REL-1.*
- **REL-2 — Sustained provider (5xx/timeout) outage → fleet-wide pipeline atoms go terminally `failed` (manual-retry-only); wiki/report runs burn retry budget.** *(major; reliability)* `classify_provider_failure` (`error.rs:125-145`) only recognizes 429/402/401-403; everything else → `Other`, which the cloud executor does NOT defer (`dispatcher.rs:520` continue; `backpressure.rs:479` Fail) and which never feeds the breaker. An outage >~6s hits every active tenant. **Fix:** add a `Transient`/`Unavailable` class (reuse `is_retryable()` at `error.rs:64-70`) so the dispatcher re-enqueues with bounded `not_before` and task_runs defer; optionally trip a short breaker pause on sustained `Other`. *Source: REL-2.*
- **DISP-2 — Slow-scan tick polls every active tenant serially before any drain.** *(major; reliability)* `dispatcher.rs:848-964` runs a serial poll loop, then `drain` once; a slow-scan tick (300s) appends every active account, so a handful of wedged tenant DBs (each ≤10s) push the whole tick — and the next drain of hinted work — into minutes. Tolerable at soft-launch scale; degrades with fleet growth. **Fix:** `buffer_unordered` bounded concurrency, or amortize the active-account scan across ticks. *Source: DISP-2.*
- **DISP-3 — AccountCache thrashes above `max_entries` because slow-scan keeps every active tenant warm (300s < 900s idle TTL).** *(major; reliability)* Above ~1000 active tenants/pod (no per-pod sharding in v1), each miss evicts an LRU tenant the next slow scan rebuilds — thundering-herd of pool opens against the shared cluster. **Fix:** a `get_for_dispatch` that doesn't refresh `last_touched`, or auto-scale `max_entries`; alert when cache is pinned at cap with no idle evictions. *Source: DISP-3.*

### UX (degradation, not broken flows)

- **PRODCLOUD-1 — Product app shows raw cloud-guard error codes (402/429) with no explanation or upgrade CTA.** *(major; ux)* `src/lib/transport/http.ts:268-276` prefers `errJson.error` (the machine CODE) over `message`, discarding the human sentence + `upgrade_url` + `Retry-After`. A free tenant hitting quota/credits/rate-limit/dunning sees a bare `quota_exceeded`/`out_of_ai_credits` token in a toast on the two highest-frequency flows (atom create, chat). **Fix:** in the cloud branch, parse 402/429 into a typed error carrying `{code, message, upgradeUrl, retryAfter}` and render a friendly banner with a `/account/billing` CTA. Nearly free — the data is already in the body. *Source: PRODCLOUD-1.*
- **DASH-1 — Global billing banner "Manage billing" link navigates straight to `/api/billing/portal`, painting raw JSON during the trial.** *(major; ux)* `BillingBanner.tsx:41-47` is a plain `<a href="/api/billing/portal">`; a trialing account (the default first-run state) has no Stripe customer, so the portal route returns `409 {"error":"no_billing_customer"}` rendered as a full-page error. **Fix:** route through `startBillingFlow`; for trialing, target checkout not portal. *Source: DASH-1.*
- **PIPE-1 — BYOK embedding-model change promises a re-embed that never runs, silently corrupting the vector space.** *(major; correctness, BYOK-only, same-dimension swaps)* `tenant_plane.rs` returns `reembed_warning()` ("until the re-embed completes") but `apply_live_config` only swaps the config slot — no re-embed is ever enqueued (cloud opens tenants in explicit-provider mode where settings writes are inert). Old/new atoms end up in mixed vector spaces, permanently degrading search/edges/wiki/chat/reports. **Fix:** enqueue a full re-embed on same-dimension model change, OR correct the warning copy to say re-embedding is not automatic / block BYOK embedding-model changes. *Source: PIPE-1.*

---

## Per-Bucket Go / No-Go

| Bucket | Verdict | Blockers | Notes |
|---|---|---|---|
| Provisioning & Lifecycle | **GO** | 0 | Provision/delete/reaper invariants sound; only minor lifecycle asymmetries (PROV-1..4, DEL-2/3). |
| Auth & Isolation | **GO** | 0 | Cross-tenant chokepoints hold (ISO-1..5, AUTH chokepoints, SEC crypto). AUTH-1 (logout) is a major fast-follow, not an isolation break. AUTH-2 dropped (false-positive). |
| Managed AI & Billing | **NO-GO** | 1 | BILL-1 (MCP guard bypass) blocks. MAI-1/DEL-1/DISP-1 are launch-critical fast-follows if a paid tier is live. |
| Data Plane & Background | **CONDITIONAL** | 0 | Provider injection & event isolation clean; PIPE-1, REL-2, DISP-2/3 are majors that degrade-but-don't-break at soft-launch scale. |
| Frontend | **CONDITIONAL** | 0 | PRODCLOUD-1, DASH-1 hurt the conversion funnel but reads/writes still work; fix before broad launch. |
| Reliability, Ops & Security | **CONDITIONAL** | 0 | No blocker, but MIG-1/REL-4 (data-safety footguns) and OPS-1/OPS-2 (no runbook) must be neutralized before standing up a production pod; REL-1 untunable chokepoint; SEC-1 SSRF. |

**Overall: CONDITIONAL-GO** — clears for soft launch once BILL-1 is fixed and the MIG-1/REL-4/OPS-1-2 operator footguns are guarded/documented. The Managed AI & Billing bucket is the only hard NO-GO and it is a single, well-scoped fix.

---

## Risk Register — Residual Risks Accepted for Launch

| ID | Risk | Why acceptable for soft launch |
|---|---|---|
| AUTH-2 (dropped) | Session cookie scoped to `.<base>` is sent to every tenant subdomain | Unreachable today: gated behind HttpOnly AND the no-tenant-active-content invariant (no `rehype-raw`/`dangerouslySetInnerHTML`). Keep "no XSS / no tenant active content on `*.<base>`" as a hard, tested, documented invariant + CSP. |
| ISO-6 | AccountCache hard-cap exceeded when every entry has a live WS subscriber | Won't bite below ~1000 connected tenants/pod. Add a per-pod WS cap + cache-len gauge as fast-follow. |
| ISO-7 | Pre-auth account state (provisioning/upgrading/suspended) distinguishable by status code | Subdomain is already semi-public; only coarse lifecycle leak, no secret. Optionally gate the `suspended` 402 behind a valid credential later. |
| REL-3 | Tenant pool fan-out (5×1000) relies on an unenforced pgbouncer assumption | Hard prerequisite, not a code bug. Document pgbouncer as required + add a boot sanity check relating `max_entries × pool × workers` to the cluster budget. |
| REL-5 | Last-writer-wins `provider_pause_kind` lets a rate-limit trip briefly re-open AI routes for an out-of-credits tenant | Self-healing on next provider call; brief window. Split the column or preserve the stronger kind if it proves noisy. |
| MAI-2 / BILL-4 | First interactive 402 not pre-empted; raw provider error until a background job trips the credits pause | Self-heals seconds-to-minutes later; CRUD unaffected. Have interactive handlers classify their own 402 as fast-follow. |
| BILL-3 | Obsidian import counts as a 1-atom delta → bounded `atom_limit` overshoot | Only reachable if a cloud route exposes `vault_path` to tenants (current product app does not). Confirm and document. |
| OPS-3 (→minor) | Logs are human-text only, no JSON / no request-correlation id | account_id IS logged in hot paths; operator can grep by tenant+timestamp. Add `--log-format json` + X-Request-Id as the observability floor. |
| OPS-4 | `--trust-proxy-header` off by default with no boot warning → per-IP limits collapse to the proxy IP behind a proxy | Per-email/per-account limiters still apply. Document as required-behind-proxy; emit a boot warn. |
| OPS-5 | `panic=abort` in server profile → any panic aborts the whole pod (all tenants) | Defensible fail-fast given durable-state recovery; orchestrator restarts. Consider `panic=unwind` for the server profile later. |
| SEC-2 | Mailgun API key can be passed on argv (`ps`/`/proc` leak) | Avoidable via env deployment. Switch to `--mailgun-api-key-env` to match the env-only discipline of every other secret. |
| DEL-2 | Deleted user's email/IP persist in `magic_links` for up to ~24h | Inert, not cross-tenant. Add `DELETE FROM magic_links WHERE lower(email)=...` to `delete_account`. |
| DEL-3 / PROV-2 | CLI `account create`/`delete` can't mint/revoke the managed key (host holds no provisioning key) → keyless accounts / orphaned keys, only a log line as trace | Operator-facing only; key allowance-capped at $0.50. Add loud notices + a durable audit surface; document CLI-vs-HTTP asymmetry. |
| PROV-1 | Reaper-resumed signups never receive the 14-day trial | Small fraction; recoverable by upgrade, data correct. Call idempotent `start_trial` from the reaper resume arm. |
| PROV-3 | Provision permit held across multi-second provision, serializing signups at 4 in-flight | Tunable: raise `--max-concurrent-provisions` + scale pods for launch spikes. |
| DASH-2 | Managed-models LLM list is a hand-maintained mirror of the server constant, no drift guard | Lists match today. Add a CI assertion or render the picker from the API. |
| DASH-3 | Suspended hold screen is a dead-end when `upgrade_url` is null | Confirm server always populates `upgrade_url`; add a `/account/billing`/support fallback. |
| PRODCLOUD-2 | Onboarding-completion write blocked by `billing_write_guard` for a returning `read_only` tenant → wizard re-loops | Not reachable at first run (trial = active). Exempt `onboarding_completed` from the write guard. |
| MIG-2 | Restore runbook stamps `last_migrated_version` at the binary target, not the dump's version → can skip migrations | Operator-mediated and rare (nightly backups are near-current). Stamp the dump's actual version and let the reaper re-migrate. |
| MIG-3 | Tenant migrations 003/005/008 omit their own `schema_version` insert | Benign today (022 writes its row, all three are idempotent). Add the missing inserts + a lint assertion. |

---

## Coverage & Not-Assessed

**Examined in depth (static analysis of the full cloud crate + relevant atomic-core/atomic-server/frontend):** provisioning & deletion ordering invariants, the reaper's six arms and advisory-lock concurrency model, magic-link/session/token chokepoints, CloudAuth host-split and account-scoped verify, the full ~78-route api_scope guard table vs `cloud_plane_guard`'s 404 set, tenant DB-name shape/SQL-injection guards, the AEAD-bound provider-credential vault and `settings_for_ai` bypass class (exhaustively grepped — no live-provider bypass), managed-key lifecycle, the Stripe webhook signature/replay/idempotency path and the dunning state machine (both clean), the quota/billing/rate-limit/breaker guards, the dispatcher/worker-pool/hint-scan fan-out, fleet-migration + additive-only lint + backup/restore credential hygiene, SQLite↔PG schema drift, the account dashboard SPA and product-app cloud-awareness gating, and the crypto/secret-custody surface.

**Verified clean (no findings) — high-confidence positives:** cross-tenant credential isolation (every verify is `WHERE account_id=$1`); AEAD ciphertext binding to (account, provider, origin); base32 tenant-DB-name injection guard; CSRF posture (SameSite=Lax + no mutating GET); Stripe webhook HMAC + idempotency; the additive-only migration lint; PGPASSWORD-in-env-not-argv; outbound TLS verification (no `danger_accept_invalid_certs`); the `atom_positions` REAL→f64 drift already fixed by migration 020; per-account WS event channel isolation (physical channel separation).

**Could not assess (caveats the operator should close out-of-band):**
- **No Postgres-gated suite was executed** (no `ATOMIC_TEST_DATABASE_URL` / live cluster) — all findings are from static reading, not runtime. The e2e/integration suites (provisioning, oauth, backup, deploy, dispatcher) should be run against a real cluster before launch.
- **Infra/deploy config outside the repo:** reverse-proxy Host-header trust (tenant routing reads raw `Host` — correct only if the proxy strips client spoofing), cluster egress posture / IMDSv1-vs-v2 hop-limit (drives SEC-1's real-world severity), wildcard DNS+TLS, S3 bucket lifecycle/retention policy (code only writes), k8s/orchestrator manifests, and pgbouncer presence/mode (REL-3) — none are in the codebase.
- **Multi-pod constraints** (cross-pod WS event relay via LISTEN/NOTIFY is not built) are a documented v1 limitation — durable state is always correct, but live WS progress events only reach the executing pod. Run a single dispatcher/serve pod until the relay lands.
- **Runtime/visual frontend behavior** was not exercised (static review only) — transient layout/responsiveness and whether the server always populates `upgrade_url` on the suspended gate (DASH-3) were not traced live.
- **Empirical thresholds** for DISP-2 (slow-scan wall time), DISP-3 (cache-thrash onset), and REL-1 (control-pool saturation under concurrency) were reasoned from structure, not measured — confirm under load test.
- **Core algorithm internals** (chunking, similarity, agent loop, `run_pipeline_jobs_batch`/`run_report` bodies) were treated as trusted per the depth boundary; only their cloud-facing outcome mapping and backpressure classification were verified.
