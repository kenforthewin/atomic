# Memory Retention Audit

## Status

Static code audit completed 2026-07-19. No runtime heap profile was captured
and no production or application code was changed as part of the audit.

## Scope

This audit investigated reports that free memory gradually decreases on the
Atomic Cloud instance until deployment, together with earlier reports of
memory pressure on self-hosted installations. It covered:

- long-lived HTTP, WebSocket, and MCP connection state;
- process-lifetime caches and maps in `atomic-server`, `atomic-core`, and
  `atomic-cloud`;
- detached background tasks and concurrency controls;
- large temporary allocations and deliberately retained caches;
- frontend subscriptions, timers, and reconnect behavior.

The findings distinguish indefinite retention from bounded backlog and RSS
high-water behavior. System free memory alone is not proof of a process heap
leak: Linux will use otherwise-idle memory for filesystem and database page
cache, and allocators do not always return freed pages to the OS immediately.

## Executive Summary

There are two credible indefinite-retention defects:

1. The WebSocket event forwarder does not consume the inbound WebSocket
   stream. A disconnected quiet client can therefore leave its forwarding
   task and broadcast receiver alive until another event is sent. On Atomic
   Cloud, that receiver makes the tenant's entire account-cache entry
   non-evictable.
2. The Streamable HTTP MCP transport creates sessions without an idle expiry.
   An abandoned client session remains in the shared session manager unless
   the client explicitly deletes it or its service terminates for another
   reason.

The WebSocket issue is the leading explanation because it exists in both
self-hosted and cloud deployments, while cloud's cache eviction policy
amplifies each stale receiver into a retained tenant manager, connection pool,
provider state, and any populated canvas cache. It is also consistent with the
slow socket/FD leak recorded in `atomic-cloud`'s process metrics commentary.

The self-hosted inline embedding pipeline presents a separate unbounded
backlog risk: work is admitted into detached tasks before the concurrency
semaphore is acquired. Canvas cache rebuilding and cloud backups can also
create large memory peaks without leaking live objects.

## Findings

| Severity | Finding | Deployment | Retention shape |
|----------|---------|------------|-----------------|
| High | WebSocket disconnects are not consumed | Both; amplified in cloud | Potentially indefinite per reconnect |
| High | MCP sessions have no idle expiry | Both | Indefinite per abandoned session |
| Medium-high | Inline pipeline admits unbounded waiting tasks | Primarily self-hosted | Grows while ingestion exceeds processing |
| Medium | Canvas debounce creates one sleeper per event | Both | Bounded by events in the last 15 seconds |
| Medium-low | Several key maps never remove old keys | Deployment-specific | Process-lifetime monotonic growth |
| Operational | Canvas and backups materialize large working sets | Deployment-specific | Peak/RSS high-water, not necessarily a leak |

## 1. WebSocket Sessions Can Outlive Their Clients

### Evidence

`atomic-server` starts a session in
`crates/atomic-server/src/ws.rs:54-89`. The call to `actix_ws::handle` returns
the response body, outbound `Session`, and inbound `MessageStream`, but the
inbound stream is bound as `_msg_stream` and immediately dropped:

```rust
let (response, mut session, _msg_stream) = actix_ws::handle(req, stream)?;
```

The spawned task then waits exclusively on `rx.recv()`. It only discovers a
closed client when a later `session.text(...)` operation fails. It never
consumes inbound Close or Ping frames, and it does not observe inbound EOF
while blocked on a quiet event channel.

The frontend reconnects automatically in
`src/lib/transport/http.ts:169-199`. A network transition or failed connection
therefore creates a new server-side receiver while an old quiet receiver may
still be present.

### Cloud amplification

`crates/atomic-cloud/src/account_cache.rs:255-259` defines an entry as
evictable only when `event_tx.receiver_count() == 0`. Both the TTL sweep and
hard-cap eviction preserve entries with receivers. This creates a logical
retention cycle:

```text
AccountCache entry owns the event sender
    -> stale WebSocket task owns a receiver
    -> receiver count prevents cache eviction
    -> cache entry keeps the tenant DatabaseManager resident
```

The retained manager can include its Postgres pools, provider configuration,
per-database cores, locks, and populated canvas caches. The documented
1,000-entry cache cap is only a target when receivers are present, so enough
stale receivers can cause the cache to exceed it.

In self-hosted mode the retained receiver and task are smaller, but the same
lifecycle defect exists. A later process-wide event may clean up a stale
receiver when its send fails; a quiet server may retain it indefinitely.

### Coverage gap

The WebSocket integration tests verify event delivery and deliberately verify
that a live receiver pins a cloud cache entry. They do not assert that the
receiver count returns to zero after a client sends Close or disconnects.

## 2. MCP Sessions Have No Idle Expiry

### Evidence

`crates/atomic-server/src/mcp/transport.rs:44-56` constructs
`LocalSessionManager::default()`. Initialization at
`transport.rs:401-450` creates a session and spawns an MCP service task. The
task removes the session only after `service.waiting()` returns. The other
explicit cleanup path is an MCP HTTP `DELETE` at `transport.rs:453-480`.

Atomic currently pins `rmcp` 0.15.0 in `Cargo.lock`. In that version,
`LocalSessionManager` stores sessions in a `HashMap`, `SessionConfig` defaults
`keep_alive` to `None`, and the local session worker converts `None` to
`Duration::MAX`. The finite initialization response ending or its HTTP client
disconnecting does not expire the newly-created session.

An orderly MCP client can issue `DELETE`, but a crash, lost network, process
kill, or client implementation that omits deletion leaves behind:

- the session map entry and handle;
- the local session worker and channels;
- the spawned MCP service task waiting for termination;
- associated handler state.

Self-hosted servers create one shared manager per process. Atomic Cloud also
constructs one MCP transport and session manager per process, shared across
all workers and tenants (`crates/atomic-cloud/src/main.rs:2688-2691`), so the
retention accumulates across accounts.

## 3. Inline Pipeline Admission Is Not Memory-Bounded

### Evidence

The default `atomic-core` save path enqueues embedding/tagging jobs and invokes
`process_queued_pipeline_jobs`; examples include atom creation and update at
`crates/atomic-core/src/lib.rs:1334-1345` and `lib.rs:1461-1470`.

`crates/atomic-core/src/embedding.rs:2079-2100` repeatedly claims every due job
into an in-memory `batches` vector. It then spawns a detached task and acquires
`EMBEDDING_BATCH_SEMAPHORE` inside that task at `embedding.rs:2126-2150`.

The semaphore bounds active batch execution to two, but does not bound task
admission. During sustained writes or a slow/hung provider, additional calls
can keep spawning futures that wait on the semaphore. Each waiting future
retains its batches, settings, storage handles, callbacks, and progress state.

Claimed jobs receive 30-minute leases before being placed behind the
semaphore. If the wait exceeds the lease, a later queue processor can reclaim
the same durable work while the original in-memory task still intends to
process it, amplifying the backlog.

Atomic Cloud disables inline execution when building tenant managers
(`crates/atomic-cloud/src/account_cache.rs:808`) and uses its bounded
dispatcher pools. This finding therefore applies primarily to self-hosted and
desktop installations.

## 4. Canvas Debouncing Bounds Rebuilds, Not Sleeping Tasks

Embedding and tagging completions are wrapped at
`crates/atomic-core/src/lib.rs:3231-3248` and call
`CanvasCache::invalidate_debounced`. Each invocation increments a generation
and spawns a new task that sleeps for 15 seconds (`lib.rs:207-242`).

Only the newest generation performs a rebuild, but superseded sleepers are not
cancelled. During a large import, memory is proportional to the number of
completion events in the previous 15 seconds. These tasks eventually finish,
so this is burst retention rather than an indefinite leak.

## 5. Process-Lifetime Maps Without Key Removal

### Setup claim limiter

`crates/atomic-server/src/state.rs:47-77` stores a `VecDeque<Instant>` for every
source IP that reaches the public setup-claim endpoint. It prunes timestamps
only for the IP currently making a request and never removes the IP key. The
limiter executes before setup-token validation and before the already-claimed
check, so an exposed self-hosted instance can accumulate keys from scanners
for its entire process lifetime.

### Cloud export managers

`crates/atomic-cloud/src/export_plane.rs:57-99` lazily creates one
`ExportJobManager` for every account that uses exports. Individual jobs and
artifacts are cleaned after their retention window, but the per-account
manager is never removed from `TenantExportPlane.managers`.

### Wiki and scheduler locks

`AtomicCore` retains one wiki mutex per tag whose article has been touched
(`crates/atomic-core/src/lib.rs:292-298`, `lib.rs:1983-1997`). The scheduler
similarly retains one mutex per `(task_id, db_id)` it encounters
(`crates/atomic-core/src/scheduler/mod.rs:117-155`). These are small and
normally bounded by logical data, but deleted/tag-churned databases and tags
do not release their keys before process restart.

## Large Allocations That Are Not Necessarily Leaks

### Global canvas cache

`AtomicCore::compute_canvas_data_impl` at
`crates/atomic-core/src/lib.rs:3284-3349` materializes all average embeddings,
the PCA projection, a position map, atom metadata, atom/tag mappings, semantic
edges, and clustering intermediates. The resulting global canvas payload is
then intentionally cached behind an `Arc`.

A debounced rebuild retains the stale payload while constructing the fresh
one, so peak live memory includes the old cache, the new result, and the
temporary working collections. Self-hosted startup also eagerly warms the
default database's cache (`crates/atomic-server/src/main.rs:690-734`). This can
raise both baseline and peak RSS as a knowledge base grows.

### Cloud backup buffers

`crates/atomic-cloud/src/backup.rs:224-259` captures the complete compressed
`pg_dump` stdout in a `Vec<u8>`. The backup pass then moves that buffer into
the selected store (`crates/atomic-cloud/src/backups.rs:687-693`). Dumps run
serially, so live memory is bounded by the largest current dump rather than
the sum of all tenant dumps, but the peak grows with database size. Allocator
high-water behavior can make the process RSS remain above its earlier level
after the buffer is dropped.

Database reads and filesystem writes can additionally increase reclaimable OS
page cache. Measurements should therefore distinguish process RSS/PSS, cgroup
memory, anonymous heap, and file cache rather than relying on system free
memory alone.

## Frontend Audit

No significant monotonically-growing browser heap path was identified.
Component-level subscriptions generally return and invoke unsubscribe
callbacks, and the reports store deliberately creates a single session-wide
subscription. `HttpTransport` retains empty event-name sets after the last
listener is removed, but the event-name universe is finite and the retained
memory is negligible.

The frontend remains relevant because its WebSocket reconnect loop can
repeatedly exercise the server-side disconnect defect.

## Runtime Validation

The static evidence is strong enough to identify defects, but production
measurements are still needed to attribute the observed slope. The most useful
correlations are:

| Signal | Interpretation |
|--------|----------------|
| `atomic_cloud_process_open_fds` rises with reconnects | Supports a socket/WebSocket lifecycle leak |
| Account-cache entries or tenant pool connections rise and do not fall after the idle TTL | Supports stale WebSocket receivers pinning tenants |
| `/proc/<pid>/fd` shows increasing socket descriptors | Confirms the FD class before a restorative restart destroys evidence |
| MCP initialize count materially exceeds session DELETE count | Supports abandoned MCP session accumulation |
| Self-hosted RSS tracks pending embedding jobs or provider outages | Supports detached inline-pipeline backlog |
| RSS steps around canvas rebuilds or nightly backups and then plateaus | Supports high-water allocation rather than indefinite live-object growth |

The cloud metrics endpoint already exposes open FDs, cache entry counts,
tenant-pool connections, worker-pool in-flight work, active export jobs, and
backup timing. It does not currently expose WebSocket receiver counts, MCP
session counts, detached inline task counts, process RSS, or allocator heap
statistics.

## Recommended Investigation Order

1. Validate WebSocket receiver cleanup and account-cache eviction after both a
   graceful Close and an ungraceful network loss.
2. Count MCP sessions and validate behavior when a client disappears without
   issuing DELETE.
3. Stress self-hosted ingestion with a deliberately stalled provider and
   observe detached task/backlog growth.
4. Measure canvas rebuild and backup peaks separately from steady-state heap.
5. Bound or sweep the smaller process-lifetime maps according to their logical
   owners.

No remediation was implemented during this audit.
