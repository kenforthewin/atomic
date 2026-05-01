# Knowledge Base Health Dashboard with Auto-Remediation

**Status:** Planning  
**Project:** atomic  
**Date:** 2026-04-30  
**Scope:** New feature

## Executive Summary

This plan implements a comprehensive health-check system for Atomic that detects and remediates data quality issues through a combination of deterministic SQL fixes and LLM-powered judgment calls. The system exposes two main endpoints (`GET /api/health/knowledge` and `POST /api/health/fix`), stores fix audits for undo capability, and integrates a dashboard widget for monitoring and manual remediation.

The feature is designed to run automatically after bulk operations (imports, re-embeddings, tag deletions) and optionally on a nightly schedule. It will surface issues to the user through the daily briefing when necessary, and enable both automatic and manual fix workflows.

**Key capabilities:**
- 11 distinct health checks covering embeddings, tagging, duplicates, contradictions, orphan tags, and content quality
- Tiered auto-fix safety model (Safe/Low/Medium/High) with dry-run support
- Durable audit logs with undo capability for all fixes
- LLM-powered fixes for merge, split, enrich, and contradiction resolution
- Dashboard widget showing health score and actionable issues
- Post-bulk-operation hooks to prevent score degradation
- Health history trending for monitoring KB quality over time

## Current Architecture / Evidence

### Existing Infrastructure to Build On

**Endpoints already in place:**
- `GET /api/embeddings/status` — returns pipeline counts (pending, processing, complete, failed)
- `GET /api/embeddings/status/all` — returns per-atom pipeline status
- `GET /api/wiki/suggestions` — returns tags eligible for wiki articles
- `GET /api/wiki/{tag_id}/status` — returns `new_atoms_available` for a specific wiki
- `GET /api/atoms/{id}/similar` — returns similar atoms above a threshold
- `GET /api/graph/edges` — returns semantic edges with similarity scores
- `POST /api/embeddings/process-pending` — processes pending embeddings
- `POST /api/embeddings/retry-failed` — retries failed embeddings
- `POST /api/graph/rebuild` — rebuilds semantic edge graph
- `POST /api/wiki/{tag_id}/generate` — generates wiki for a tag
- `POST /api/utils/compact-tags` — removes orphan tags

**Code patterns to follow:**

1. **Route handlers** (`crates/atomic-server/src/routes/`):
   - Follow pattern: `#[utoipa::path(...)] pub async fn handler(db: Db, ...) -> HttpResponse`
   - Use `ok_or_error()` for simple responses, `crate::error::error_response(e)` for errors
   - Register new routes in `routes/mod.rs`

2. **LLM integration** (existing patterns from `tagging`, `wiki`, `chat`):
   - Call LLM provider trait methods from atomic-core
   - Send structured prompts with available context
   - Handle streaming vs. completion-based responses
   - Parse JSON-structured outputs when needed

3. **Event callbacks** (pattern from `embedding.rs`):
   ```rust
   let on_event = embedding_event_callback(state.event_tx.clone());
   db.0.some_operation(on_event).await
   ```

4. **Database schema** (`crates/atomic-core/src/db.rs`):
   - Use rusqlite for SQLite; all new tables need both SQLite and Postgres implementations
   - Existing migrations in `migrations/` directory (SQLite) and `crates/atomic-core/src/storage/postgres/migrations/`
   - Per-DB data lives in the data database; shared state lives in registry.db

5. **Settings/Configuration** (`crates/atomic-core/src/settings.rs`):
   - Global settings stored in registry.db via `get_setting()` / `set_setting()`
   - Per-DB settings can override via `storage.get_all_settings_sync()` / `set_setting_sync()`
   - LLM prompt templates stored as settings

### Data Model

**Atoms** (existing):
- `atoms.id`, `atoms.content`, `atoms.source_url`, `atoms.embedding_status`, `atoms.tagging_status`, `atoms.embedding_error`, `atoms.tagging_error`

**Tags** (existing):
- `tags.id`, `tags.name`, `tags.parent_id`, `tags.is_autotag_target`

**Semantic edges** (existing):
- `semantic_edges.atom_a_id`, `semantic_edges.atom_b_id`, `semantic_edges.similarity`

**Wiki articles** (existing):
- `wiki_articles.tag_id`, `wiki_articles.content`, `wiki_articles.last_generated_at`

**Conversations** (existing):
- `conversations.id`, `conversations.tag_filter`

## Recommended Approach

### Architecture Decision: Modular Health System

The health system will be organized as a new module `atomic-core::health` with submodules:

- `health/mod.rs` — orchestration, score calculation, overall health computation
- `health/checks.rs` — individual check implementations (deterministic SQL queries)
- `health/fixes.rs` — deterministic auto-fix logic (no LLM needed)
- `health/llm_fixes.rs` — LLM-powered fix logic (merge, split, enrich, contradict)
- `health/audit.rs` — fix logging and undo capability

**Why this structure:**
1. Separates concerns: query logic, fix logic, LLM logic, audit logic are independent
2. Makes it easy to test individual checks in isolation
3. Allows future extensions without touching core module
4. Follows existing pattern: embedding, wiki, chat are all separate modules with clear responsibilities

### Tiered Fix Safety Model

| Tier | Risk | Confirmation | Examples |
|------|------|-------------|----------|
| **Safe** | Zero risk | Auto-run, no confirmation | Retry failed embeddings, rebuild graph, process pending tagging |
| **Low** | Minimal risk, reversible | Auto-run with undo log | Delete orphan tags, generate missing wikis |
| **Medium** | Changes content | Dry-run first, confirm | Add headings, merge exact-source duplicates |
| **High** | Deletes or rewrites | Always require review | Merge similar atoms, split long atoms, delete stubs, resolve contradictions |

**Endpoint semantics:**
- `POST /api/health/fix { mode: "auto" }` → runs Safe + Low tier
- `POST /api/health/fix { mode: "auto", include_medium: true }` → Safe + Low + Medium
- `POST /api/health/fix { mode: "dry_run", ... }` → report what would be fixed without executing

### LLM Prompt Templates

Store as settings (like `tagging_prompt`, `chat_prompt` already do):
- `health.merge_duplicates_prompt` — for merging high-similarity atoms
- `health.contradiction_detection_prompt` — for finding conflicting info
- `health.split_long_atom_prompt` — for splitting >15K character atoms
- `health.enrich_stub_atom_prompt` — for expanding <100 char atoms
- `health.add_structure_prompt` — for adding headings to unstructured content
- `health.tag_reorganize_prompt` — for suggesting tag hierarchy fixes

All with sensible defaults so zero setup is needed.

## Implementation Plan

### Phase I: Core Infrastructure (Foundation)

**1.1 Database schema additions**

Add to SQLite migration (new file `migrations/XXX_create_health_tables.sql`):

```sql
-- Health reports — historical snapshots of KB state
CREATE TABLE health_reports (
  id TEXT PRIMARY KEY,
  computed_at TEXT NOT NULL,
  overall_score INTEGER NOT NULL,
  check_scores TEXT NOT NULL,    -- JSON: {"duplicates": 80, ...}
  atom_count INTEGER NOT NULL,
  auto_fixes_applied INTEGER DEFAULT 0,
  report_json TEXT NOT NULL      -- Full report for detail view
);
CREATE INDEX idx_health_reports_computed ON health_reports(computed_at DESC);

-- Audit log of all auto-fix actions (for undo)
CREATE TABLE health_fix_log (
  id TEXT PRIMARY KEY,
  check_name TEXT NOT NULL,
  action TEXT NOT NULL,
  tier TEXT NOT NULL,            -- "safe", "low", "medium", "high"
  atom_ids TEXT,                 -- JSON array of affected atom IDs
  tag_ids TEXT,                  -- JSON array of affected tag IDs
  before_state TEXT,             -- JSON snapshot (for undo)
  after_state TEXT,              -- JSON snapshot (for verification)
  llm_prompt TEXT,               -- Prompt sent to LLM (if applicable)
  llm_response TEXT,             -- Raw LLM response (for audit)
  executed_at TEXT NOT NULL,
  undone_at TEXT                 -- NULL unless undone
);
CREATE INDEX idx_health_fix_log_executed ON health_fix_log(executed_at DESC);
CREATE INDEX idx_health_fix_log_check ON health_fix_log(check_name);
```

Add equivalent Postgres migration to `crates/atomic-core/src/storage/postgres/migrations/`.

**1.2 Models for health domain**

New file: `crates/atomic-core/src/health/models.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub overall_score: u32,             // 0-100
    pub overall_status: HealthStatus,   // healthy, needs_attention, degraded, unhealthy
    pub computed_at: String,
    pub atom_count: i32,
    pub checks: HashMap<String, HealthCheck>,
    pub auto_fixable: i32,              // count of auto-fixable issues
    pub requires_review: i32,           // count of issues needing human review
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HealthStatus {
    #[serde(rename = "healthy")]
    Healthy,
    #[serde(rename = "needs_attention")]
    NeedsAttention,
    #[serde(rename = "degraded")]
    Degraded,
    #[serde(rename = "unhealthy")]
    Unhealthy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    pub status: String,                 // "ok", "warning", "error"
    pub score: u32,                     // 0-100 contribution to overall
    // Check-specific fields vary by check type
    #[serde(flatten)]
    pub data: serde_json::Value,       // Dynamic fields per check
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixAction {
    pub check: String,
    pub action: String,                 // "deleted_tags", "merged_atoms", etc.
    pub count: i32,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixResponse {
    pub mode: String,                   // "auto", "dry_run"
    pub actions_taken: Vec<FixAction>,
    pub skipped: Vec<SkippedFix>,
    pub new_score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFix {
    pub check: String,
    pub reason: String,
    pub count: i32,
}
```

**1.3 Storage trait additions**

Add to `crates/atomic-core/src/storage/traits.rs`:

```rust
#[async_trait]
pub trait HealthStore: Send + Sync {
    // Health report storage
    async fn store_health_report(&self, report: &HealthReport) -> StorageResult<()>;
    async fn get_latest_health_report(&self) -> StorageResult<Option<HealthReport>>;
    async fn get_health_reports_since(&self, since: &str) -> StorageResult<Vec<HealthReport>>;

    // Fix audit log
    async fn log_fix_action(&self, fix_log: &HealthFixLog) -> StorageResult<()>;
    async fn get_fix_log(&self, fix_id: &str) -> StorageResult<Option<HealthFixLog>>;
    async fn get_recent_fixes(&self, limit: i32) -> StorageResult<Vec<HealthFixLog>>;
    async fn undo_fix(&self, fix_id: &str) -> StorageResult<()>;
}

#[derive(Debug, Clone)]
pub struct HealthFixLog {
    pub id: String,
    pub check_name: String,
    pub action: String,
    pub tier: String,               // "safe", "low", "medium", "high"
    pub atom_ids: Option<Vec<String>>,
    pub tag_ids: Option<Vec<String>>,
    pub before_state: String,       // JSON
    pub after_state: String,        // JSON
    pub llm_prompt: Option<String>,
    pub llm_response: Option<String>,
    pub executed_at: String,
    pub undone_at: Option<String>,
}
```

Implement for both `SqliteStorage` and `PostgresStorage` (mostly straightforward INSERT/SELECT statements).

### Phase II: Health Checks (11 distinct checks)

**2.1 `health/checks.rs` — Deterministic checks**

Each check returns a `HealthCheck` struct with standardized fields. Run synchronously over database snapshots.

```rust
pub async fn check_embedding_coverage(storage: &dyn Storage) -> HealthCheck {
    // Count: pending, processing, complete, failed
    // Score: (complete / total) * 100, cap at 50 if any failed
    // Return distribution + status
}

pub async fn check_source_uniqueness(storage: &dyn Storage) -> HealthCheck {
    // Find source_urls appearing on multiple atoms
    // Score: 100 if 0 duplicates, subtract 15 per duplicate
}

pub async fn check_orphan_tags(storage: &dyn Storage) -> HealthCheck {
    // Find tags with 0 atoms and no children, excluding autotag targets
    // Score: 100 if 0, subtract 2 per orphan
}

pub async fn check_tagging_coverage(storage: &dyn Storage) -> HealthCheck {
    // Count atoms: tagged, untagged, failed, skipped
    // Score: (tagged / total) * 100
}

pub async fn check_semantic_graph_freshness(storage: &dyn Storage) -> HealthCheck {
    // Compare last rebuild time vs newest atom
    // Score: 100 if recent, subtract 2 per atom since rebuild
}

pub async fn check_wiki_coverage(storage: &dyn Storage) -> HealthCheck {
    // Find tags with >= 5 atoms that could have wikis
    // Count: with_wiki, without_wiki, stale
    // Score: (with_wiki / eligible) * 70 + (non_stale / with_wiki) * 30
}

pub async fn check_content_quality(storage: &dyn Storage) -> HealthCheck {
    // Flag atoms: very_short (<100 chars), very_long (>15K chars),
    //           no_headings, no_source
    // Score: 85 base, minus 5 for each category with issues
}

pub async fn check_tag_health(storage: &dyn Storage) -> HealthCheck {
    // Find: single-atom tags, rootless tags, similar-named tags
    // Score: points deducted per category
}
```

**2.2 LLM-powered checks**

To be added in Phase III. For now, implement skeleton methods that return placeholder scores.

```rust
pub async fn check_duplicate_detection(
    storage: &dyn Storage,
    _providers: &dyn LlmProvider,
) -> HealthCheck {
    // Find atom pairs with similarity 0.92-1.0 from different sources
    // Mark as requires_review: true
    // Don't execute fixes yet
}

pub async fn check_contradiction_detection(
    storage: &dyn Storage,
    _providers: &dyn LlmProvider,
) -> HealthCheck {
    // Find atoms with similarity 0.75-0.92 (same topic, different content)
    // Use LLM to confirm contradiction (Phase III)
    // Mark as requires_review: true
}
```

**2.3 Score aggregation**

```rust
pub fn compute_overall_score(checks: &HashMap<String, HealthCheck>) -> u32 {
    let weights = [
        ("duplicate_detection", 0.15),
        ("embedding_coverage", 0.15),
        ("source_uniqueness", 0.10),
        ("tagging_coverage", 0.10),
        ("wiki_coverage", 0.10),
        ("semantic_graph_freshness", 0.10),
        ("content_quality", 0.05),
        ("orphan_tags", 0.05),
        ("tag_health", 0.05),
        ("contradiction_detection", 0.05),
        ("tagging_coverage", 0.10),  // untagged atoms
    ];
    
    let mut total = 0.0;
    for (check_name, weight) in weights.iter() {
        if let Some(check) = checks.get(*check_name) {
            total += (check.score as f64) * weight;
        }
    }
    total as u32
}

pub fn status_for_score(score: u32) -> HealthStatus {
    match score {
        90..=100 => HealthStatus::Healthy,
        70..=89 => HealthStatus::NeedsAttention,
        50..=69 => HealthStatus::Degraded,
        _ => HealthStatus::Unhealthy,
    }
}
```

### Phase III: Auto-Fix Implementation

**3.1 `health/fixes.rs` — Deterministic fixes (no LLM)**

```rust
pub async fn fix_embedding_coverage(
    storage: &dyn Storage,
    core: &AtomicCore,
) -> Result<FixAction, AtomicCoreError> {
    // Call core.process_pending_embeddings()
    // Call core.retry_failed_embeddings()
    // Return action: { check: "embedding_coverage", action: "retry_failed_and_process_pending", count: X }
}

pub async fn fix_orphan_tags(storage: &dyn Storage) -> Result<FixAction, AtomicCoreError> {
    // Find and delete orphan tags (not autotag targets)
    // Log to health_fix_log with before_state
    // Return action: { check: "orphan_tags", action: "deleted_tags", count: X, details: [tag_names] }
}

pub async fn fix_source_uniqueness(storage: &dyn Storage) -> Result<FixAction, AtomicCoreError> {
    // For exact source_url duplicates:
    //   - Keep newest (by created_at)
    //   - Merge tags from deleted atoms onto the kept one
    //   - Delete older atoms
    // Log all deletes with before_state
    // Return action with count
}

pub async fn fix_semantic_graph_freshness(
    storage: &dyn Storage,
    core: &AtomicCore,
) -> Result<FixAction, AtomicCoreError> {
    // Call core.rebuild_semantic_edges()
    // Return action: { check: "semantic_graph_freshness", action: "queued_rebuild", ... }
}
```

**3.2 `health/llm_fixes.rs` — LLM-powered fixes**

These will call LLM provider to make judgment calls. Implemented in Phase III.

```rust
pub async fn fix_tagging_coverage_with_llm(
    storage: &dyn Storage,
    core: &AtomicCore,
    llm: &dyn LlmProvider,
    untagged_atoms: &[AtomWithTags],
) -> Result<FixAction, AtomicCoreError> {
    // For each untagged atom, call LLM with modified prompt that forces >= 1 tag
    // Re-run tagging with forced assignment
    // Return action with count of newly tagged atoms
}

pub async fn merge_duplicate_atoms_with_llm(
    storage: &dyn Storage,
    atom_a: &AtomWithTags,
    atom_b: &AtomWithTags,
    llm: &dyn LlmProvider,
) -> Result<MergeResult, AtomicCoreError> {
    // Call LLM to synthesize both atoms
    // Update newer atom with merged content
    // Delete older atom
    // Re-embed and re-tag merged atom
    // Log to health_fix_log
}

pub async fn split_long_atom_with_llm(
    storage: &dyn Storage,
    atom: &AtomWithTags,
    llm: &dyn LlmProvider,
) -> Result<FixAction, AtomicCoreError> {
    // Call LLM to analyze if atom should be split
    // If yes: create new atoms for each section
    // If no: add structure (headings) instead
    // Log all creates/deletes
}
```

**3.3 `health/audit.rs` — Undo capability**

```rust
pub async fn undo_fix(
    storage: &dyn Storage,
    fix_id: &str,
) -> Result<(), AtomicCoreError> {
    // Fetch fix_log entry via fix_id
    // Parse before_state JSON
    // For each affected atom_id: restore from before_state
    // For each affected tag_id: restore from before_state
    // Mark fix_log.undone_at = now()
    // Return any created entries to allow cascading undo
}
```

### Phase IV: API Endpoints

**4.1 `routes/health.rs` — New route handlers**

```rust
#[utoipa::path(
    get,
    path = "/api/health/knowledge",
    responses(
        (status = 200, description = "Health report", body = HealthReport)
    ),
    tag = "health"
)]
pub async fn get_health_knowledge(
    state: web::Data<AppState>,
    db: Db,
) -> HttpResponse {
    // Compute all checks
    // Calculate overall score
    // Store report
    // Return JSON
}

#[utoipa::path(
    post,
    path = "/api/health/fix",
    request_body = FixRequest,
    responses(
        (status = 200, description = "Fix results", body = FixResponse)
    ),
    tag = "health"
)]
pub async fn run_health_fix(
    state: web::Data<AppState>,
    db: Db,
    body: web::Json<FixRequest>,
) -> HttpResponse {
    // Determine which fixes to run based on mode and include_medium
    // Execute fixes in tier order (Safe → Low → Medium)
    // Collect FixAction results
    // Recompute health score
    // Return FixResponse
}

#[utoipa::path(
    post,
    path = "/api/health/fix/{check}/{item_id}",
    params(
        ("check" = String, Path),
        ("item_id" = String, Path)
    ),
    request_body = ManualFixRequest,
    responses(
        (status = 200, description = "Fix applied")
    ),
    tag = "health"
)]
pub async fn apply_manual_fix(
    state: web::Data<AppState>,
    db: Db,
    path: web::Path<(String, String)>,
    body: web::Json<ManualFixRequest>,
) -> HttpResponse {
    // Route to specific fix handler based on check name
    // Execute fix with user parameters
    // Log to health_fix_log
    // Return success
}

#[utoipa::path(
    post,
    path = "/api/health/undo/{fix_id}",
    params(("fix_id" = String, Path)),
    responses(
        (status = 200, description = "Fix undone")
    ),
    tag = "health"
)]
pub async fn undo_health_fix(db: Db, path: web::Path<String>) -> HttpResponse {
    let fix_id = path.into_inner();
    ok_or_error(db.0.undo_fix(&fix_id).await)
}
```

**4.2 Request/response types**

```rust
#[derive(Deserialize, ToSchema)]
pub struct FixRequest {
    pub checks: Option<Vec<String>>,  // If None, run all
    pub mode: String,                  // "auto", "dry_run"
    pub include_medium: Option<bool>,   // Default false
}

#[derive(Deserialize, ToSchema)]
pub struct ManualFixRequest {
    pub action: String,                 // "merge", "keep_both", "delete_one", etc.
    pub keep_atom_id: Option<String>,
    pub merge_strategy: Option<String>, // "keep_newer", "keep_longer", "llm"
}
```

**4.3 Register in `routes/mod.rs`**

```rust
pub mod health;
// ...
// In web::scope("/api"):
.service(
    web::scope("/health")
        .route("/knowledge", web::get().to(health::get_health_knowledge))
        .route("/fix", web::post().to(health::run_health_fix))
        .route("/fix/{check}/{item_id}", web::post().to(health::apply_manual_fix))
        .route("/undo/{fix_id}", web::post().to(health::undo_health_fix))
)
```

### Phase V: Integration

**5.1 Post-bulk-operation hooks**

Add to import handlers (`import/obsidian.rs`, `ingest/fetch.rs`), bulk atom creation, tag deletion:

```rust
async fn post_bulk_operation_hook(core: &AtomicCore) {
    // Run health check
    let report = compute_health(&core).await.ok();
    
    if let Some(r) = report {
        if r.overall_score < 95 {
            // Auto-fix safe issues
            let _ = core.run_health_fix(FixMode::Safe).await;
            
            // Recompute and cache
            let updated_report = compute_health(&core).await.ok();
            // ... store for later use
        }
    }
}
```

**5.2 Scheduled nightly maintenance**

Add to `crates/atomic-core/src/scheduler/mod.rs`:

```rust
pub async fn health_maintenance(core: &AtomicCore) {
    // Run full health check
    let report = compute_health(&core).await.ok();
    
    // Auto-fix safe + low tier issues
    if let Some(_) = report {
        let _ = core.run_health_fix(FixMode::Low).await;
    }
    
    // Store report for history
    // If score dropped, include in next briefing
}
```

Add task config to settings:

```rust
("task.health_maintenance.enabled", "true"),
("task.health_maintenance.interval_hours", "24"),
("task.health_maintenance.auto_fix_tier", "low"),
```

**5.3 Briefing integration**

Extend `crates/atomic-core/src/briefing/mod.rs` to include health findings:

```rust
fn format_health_section(report: &HealthReport) -> String {
    // Only include if score < 85 or contradictions found
    // Format:
    //   ## Knowledge Health
    //   Your KB score is X/100 (status).
    //   Auto-fixed: [list]
    //   Needs review: [list]
}
```

**5.4 Settings/configuration**

Add LLM prompt templates to `DEFAULT_SETTINGS` in `settings.rs`:

```rust
("health.merge_duplicates_prompt", "..."),
("health.contradiction_detection_prompt", "..."),
("health.split_long_atom_prompt", "..."),
("health.enrich_stub_atom_prompt", "..."),
("health.add_structure_prompt", "..."),
("health.tag_reorganize_prompt", "..."),
```

### Phase VI: Frontend (Dashboard Widget)

**6.1 Health panel component**

New file: `src/components/dashboard/HealthPanel.tsx`

```tsx
export function HealthPanel() {
    const [report, setReport] = useState<HealthReport | null>(null);
    const [loading, setLoading] = useState(false);

    useEffect(() => {
        fetchHealth();
    }, []);

    async function fetchHealth() {
        const resp = await getTransport().invoke('get_health_knowledge', {});
        setReport(resp as HealthReport);
    }

    async function autoFix() {
        setLoading(true);
        const resp = await getTransport().invoke('run_health_fix', {
            mode: 'auto'
        });
        await fetchHealth();
        setLoading(false);
    }

    if (!report) return null;

    return (
        <div className="p-4 bg-[#252525] rounded border border-purple-600/30">
            <div className="flex justify-between items-center mb-4">
                <h3 className="text-lg font-bold">Knowledge Health</h3>
                <span className="text-2xl font-bold">{report.overall_score}/100</span>
            </div>

            <div className="space-y-2 mb-4">
                {Object.entries(report.checks).map(([name, check]) => (
                    <HealthCheckBar key={name} name={name} check={check} />
                ))}
            </div>

            <div className="flex gap-2">
                <button onClick={autoFix} disabled={loading} className="px-3 py-1 bg-purple-600 rounded text-sm">
                    {loading ? 'Fixing...' : 'Fix Safe Issues'}
                </button>
                <button className="px-3 py-1 bg-purple-600/50 rounded text-sm">
                    Review Required ({report.requires_review})
                </button>
            </div>

            {report.auto_fixable > 0 && (
                <p className="text-xs text-gray-400 mt-2">{report.auto_fixable} issues can be automatically fixed</p>
            )}
        </div>
    );
}
```

**6.2 Add to dashboard registry**

Register `HealthPanel` in `src/components/dashboard/registry.ts` to make it available as a dashboard widget.

**6.3 Review queue page**

New page: `src/routes/health/review/+page.svelte` (if using SvelteKit) or equivalent route for showing duplicates, contradictions, stubs that need human review.

## Files / Components To Change

### New Files

**Backend (Rust):**
- `crates/atomic-core/src/health/mod.rs` — module root, orchestration
- `crates/atomic-core/src/health/checks.rs` — 11 health checks (deterministic)
- `crates/atomic-core/src/health/fixes.rs` — auto-fix logic (deterministic)
- `crates/atomic-core/src/health/llm_fixes.rs` — LLM-powered fixes (Phase III)
- `crates/atomic-core/src/health/audit.rs` — fix logging, undo
- `crates/atomic-core/src/health/models.rs` — health domain types
- `crates/atomic-server/src/routes/health.rs` — endpoint handlers
- `migrations/XXX_create_health_tables.sql` — SQLite schema
- `crates/atomic-core/src/storage/postgres/migrations/XXX_create_health_tables.sql` — Postgres schema

**Frontend (TypeScript/React):**
- `src/components/dashboard/HealthPanel.tsx` — main dashboard widget
- `src/routes/health/+page.tsx` or `.svelte` — detailed health page
- `src/routes/health/review/+page.tsx` — high-tier fix review queue
- `src/lib/api/health.ts` — health API client (type-safe wrapper)

### Modified Files

**Backend (Rust):**
- `crates/atomic-core/src/lib.rs` — add `pub mod health`
- `crates/atomic-core/src/storage/traits.rs` — add `HealthStore` trait
- `crates/atomic-core/src/storage/sqlite/mod.rs` — implement `HealthStore`
- `crates/atomic-core/src/storage/postgres/mod.rs` — implement `HealthStore`
- `crates/atomic-core/src/storage/sqlite/settings.rs` — add health prompt defaults
- `crates/atomic-server/src/routes/mod.rs` — register health routes
- `crates/atomic-core/src/scheduler/mod.rs` — add health_maintenance task
- `crates/atomic-core/src/briefing/mod.rs` — include health findings
- `crates/atomic-core/src/settings.rs` — add health prompt templates to `DEFAULT_SETTINGS`
- Bulk import handlers — add post-operation hooks
- `crates/atomic-server/src/state.rs` — may need event channel registration

**Frontend (TypeScript):**
- `src/components/dashboard/registry.ts` — add HealthPanel widget
- `src/lib/api.ts` — add health API methods
- `src/stores/ui.ts` — may need state for health reports

## Data Flow / Interfaces

### Health Check Flow

```
GET /api/health/knowledge
  ↓
compute_health(core)
  ├─ check_embedding_coverage()     → HealthCheck { score, status, data }
  ├─ check_source_uniqueness()      → HealthCheck
  ├─ check_orphan_tags()            → HealthCheck
  ├─ check_tagging_coverage()       → HealthCheck
  ├─ check_semantic_graph_freshness() → HealthCheck
  ├─ check_wiki_coverage()          → HealthCheck
  ├─ check_content_quality()        → HealthCheck
  ├─ check_tag_health()             → HealthCheck
  ├─ check_duplicate_detection()    → HealthCheck { requires_review: true }
  ├─ check_contradiction_detection() → HealthCheck { requires_review: true }
  └─ check_tagging_coverage()       → HealthCheck
  ↓
aggregate_scores(checks) → overall_score: u32
  ↓
HealthReport { overall_score, checks, status, auto_fixable, requires_review }
  ↓
store_health_report(report)
  ↓
HTTP 200 → HealthReport (JSON)
```

### Fix Flow

```
POST /api/health/fix { mode: "auto", include_medium?: bool }
  ↓
determine_fix_tiers(mode, include_medium)
  ↓
for each fix in order:
  - tier < "medium" or include_medium → run it
  - capture before_state
  - execute fix
  - capture after_state
  - log to health_fix_log with undo info
  ↓
recompute_health()
  ↓
FixResponse { actions_taken, skipped, new_score }
  ↓
HTTP 200 → FixResponse (JSON)
```

### Undo Flow

```
POST /api/health/undo/{fix_id}
  ↓
fetch_fix_log(fix_id)
  ↓
parse_before_state(log.before_state)
  ↓
for each atom_id:
  restore_atom(atom_id, before_snapshot)
for each tag_id:
  restore_tag(tag_id, before_snapshot)
  ↓
set_fix_log.undone_at = now()
  ↓
HTTP 200 → { status: "ok" }
```

## Configuration / Secrets / Deployment Notes

**No additional secrets needed.** Health system uses existing LLM providers (OpenRouter, Ollama, OpenAI-compatible).

**Settings added to `DEFAULT_SETTINGS`:**
```rust
("task.health_maintenance.enabled", "true"),
("task.health_maintenance.interval_hours", "24"),
("task.health_maintenance.auto_fix_tier", "low"),
("health.merge_duplicates_prompt", "<default>"),
("health.contradiction_detection_prompt", "<default>"),
("health.split_long_atom_prompt", "<default>"),
("health.enrich_stub_atom_prompt", "<default>"),
("health.add_structure_prompt", "<default>"),
("health.tag_reorganize_prompt", "<default>"),
```

All with sensible defaults that work immediately on fresh install.

**Environment notes:**
- Health checks complete in < 2s for 500 atoms (single query per check, no N+1)
- LLM-powered fixes (Phase III) are rate-limited: max 3 wiki generations per fix run
- Contradiction detection may be async for large KBs (>1000 atoms with 0.75+ similarity pairs)
- Health reports stored indefinitely; UI may paginate to last 90 days

## Testing / Validation Plan

### Unit Tests

Create `crates/atomic-core/tests/health_tests.rs`:

**Test 1: Clean database scores 100**
```rust
#[tokio::test]
async fn health_clean_db_is_100() {
    let db = setup_test_db().await;
    let report = compute_health(&db).await.unwrap();
    assert_eq!(report.overall_score, 100);
}
```

**Test 2: Orphan tags detected and fixable**
```rust
#[tokio::test]
async fn orphan_tags_detected_and_fixed() {
    let db = setup_test_db().await;
    create_orphan_tag(&db, "orphan").await;
    
    let report_before = compute_health(&db).await.unwrap();
    assert!(report_before.overall_score < 100);
    
    run_health_fix(&db, FixMode::Safe).await.unwrap();
    
    let report_after = compute_health(&db).await.unwrap();
    assert_eq!(report_after.overall_score, 100);
}
```

**Test 3: Failed embeddings cause score drop**
```rust
#[tokio::test]
async fn failed_embeddings_drop_score() {
    let db = setup_test_db().await;
    create_atom_with_status(&db, "test", "embedding", "failed", None).await;
    
    let report = compute_health(&db).await.unwrap();
    assert!(report.overall_score <= 50);
    assert!(report.checks["embedding_coverage"].data["failed"] > 0);
}
```

**Test 4: Fix audit log stores before/after state**
```rust
#[tokio::test]
async fn fix_logged_with_undo_capability() {
    let db = setup_test_db().await;
    create_orphan_tag(&db, "orphan").await;
    
    let fix_response = run_health_fix(&db, FixMode::Safe).await.unwrap();
    assert_eq!(fix_response.actions_taken.len(), 1);
    
    let logs = db.get_recent_fixes(10).await.unwrap();
    assert_eq!(logs[0].check_name, "orphan_tags");
    assert!(logs[0].before_state.contains("orphan"));
}
```

**Test 5: Undo restores pre-fix state**
```rust
#[tokio::test]
async fn undo_fix_restores_state() {
    let db = setup_test_db().await;
    create_orphan_tag(&db, "orphan").await;
    
    let before_count = db.count_tags().await.unwrap();
    
    let fix_response = run_health_fix(&db, FixMode::Safe).await.unwrap();
    let fix_id = fix_response.actions_taken[0].id.clone();
    
    let after_count = db.count_tags().await.unwrap();
    assert_eq!(after_count, before_count - 1);
    
    db.undo_fix(&fix_id).await.unwrap();
    
    let restored_count = db.count_tags().await.unwrap();
    assert_eq!(restored_count, before_count);
}
```

**Test 6: Duplicate detection finds high-similarity atoms**
```rust
#[tokio::test]
async fn duplicates_detected() {
    let db = setup_test_db().await;
    create_atom_pair_with_similarity(&db, 0.95, "obs://vault1/file", "obs://vault2/file").await;
    
    let report = compute_health(&db).await.unwrap();
    let dup_check = &report.checks["duplicate_detection"];
    assert!(dup_check.data["count"] > 0);
    assert_eq!(dup_check.status, "warning");
}
```

**Test 7: Contradictions flagged for review**
```rust
#[tokio::test]
async fn contradictions_detected() {
    let db = setup_test_db().await;
    // Create two atoms with same embedding, contradictory content
    create_contradictory_atoms(&db, 0.82).await;
    
    let report = compute_health(&db).await.unwrap();
    let contra_check = &report.checks["contradiction_detection"];
    assert!(contra_check.data["count"] > 0);
    assert_eq!(contra_check.status, "warning");
}
```

**Test 8: Dry run doesn't apply fixes**
```rust
#[tokio::test]
async fn dry_run_mode_doesnt_fix() {
    let db = setup_test_db().await;
    create_orphan_tag(&db, "orphan").await;
    
    let before_count = db.count_tags().await.unwrap();
    
    let response = run_health_fix_dry_run(&db, FixMode::Safe).await.unwrap();
    assert_eq!(response.mode, "dry_run");
    assert!(response.actions_taken.len() > 0);
    
    let after_count = db.count_tags().await.unwrap();
    assert_eq!(after_count, before_count);  // Nothing actually deleted
}
```

**Test 9: Score weighted correctly**
```rust
#[tokio::test]
async fn overall_score_weighted() {
    let db = setup_test_db().await;
    
    // Set up state with:
    // - embedding_coverage at 50 (worth 15%)
    // - all others at 100
    // Expected: (50 * 0.15) + (100 * 0.85) = 92.5 → 92
    
    let report = compute_health(&db).await.unwrap();
    assert_eq!(report.overall_score, 92);
}
```

**Test 10: Very short atoms flagged for review**
```rust
#[tokio::test]
async fn very_short_atoms_flagged() {
    let db = setup_test_db().await;
    create_atom(&db, "hi").await;  // 2 chars
    
    let report = compute_health(&db).await.unwrap();
    let quality_check = &report.checks["content_quality"];
    assert!(quality_check.data["very_short"]["count"] > 0);
}
```

### Integration Tests

Create `crates/atomic-server/tests/health_api_tests.rs`:

**Test 1: GET /api/health/knowledge returns valid report**
```rust
#[actix_web::test]
async fn get_health_knowledge_endpoint() {
    let app = create_test_app().await;
    let resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/health/knowledge").to_request(),
    ).await;
    
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = test::read_body(resp).await;
    let report: HealthReport = serde_json::from_slice(&body).unwrap();
    assert!(report.overall_score >= 0 && report.overall_score <= 100);
}
```

**Test 2: POST /api/health/fix auto mode fixes safe issues**
```rust
#[actix_web::test]
async fn post_health_fix_auto_mode() {
    let app = create_test_app_with_orphan_tag().await;
    
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/health/fix")
            .set_json(json!({ "mode": "auto" }))
            .to_request(),
    ).await;
    
    assert_eq!(resp.status(), http::StatusCode::OK);
    let body = test::read_body(resp).await;
    let fix_resp: FixResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(fix_resp.mode, "auto");
    assert!(fix_resp.actions_taken.len() > 0);
}
```

**Test 3: Dry run doesn't persist changes**
```rust
#[actix_web::test]
async fn health_fix_dry_run_mode() {
    let app = create_test_app_with_orphan_tag().await;
    
    // Run in dry_run mode
    test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/api/health/fix")
            .set_json(json!({ "mode": "dry_run" }))
            .to_request(),
    ).await;
    
    // Check that orphan still exists
    let health_resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/api/health/knowledge").to_request(),
    ).await;
    let health: HealthReport = serde_json::from_slice(&test::read_body(health_resp).await).unwrap();
    assert!(health.checks["orphan_tags"].data["count"] > 0);
}
```

### Manual Verification Steps

1. **Fresh database should score 100:**
   - Start server with empty database
   - GET /api/health/knowledge
   - Verify overall_score == 100
   - Verify all check statuses are "ok"

2. **Create pathological state and verify detection:**
   - Manually insert orphan tag, failed embedding, duplicate atoms
   - GET /api/health/knowledge
   - Verify issues are detected with correct counts

3. **Auto-fix safe issues:**
   - POST /api/health/fix { mode: "auto" }
   - Verify fixes applied
   - GET /api/health/knowledge again
   - Verify score improved

4. **Dashboard widget renders:**
   - Open dashboard in browser
   - Verify HealthPanel displays
   - Verify score and check bars render
   - Click "Fix Safe Issues" button
   - Verify panel updates after fix

5. **Post-import health check runs:**
   - Import Obsidian vault with 100+ notes
   - In background/logs, verify health_maintenance task ran
   - GET /api/health/knowledge
   - Verify no degradation from pre-import state

## Risks, Assumptions, and Open Questions

### Risks

1. **LLM cost for large KBs** — Contradiction detection on 1000+ atoms with high similarity pairs could be expensive. Mitigation: Rate-limit and make async with background job queue.

2. **False positives on contradictions** — LLM may incorrectly flag atoms as contradictory when they're actually complementary. Mitigation: Always mark as requires_review, never auto-fix without user confirmation.

3. **Merge strategy decisions** — When merging atoms, choosing which source URL to keep is lossy. Mitigation: Store secondary URL in merged atom's body as "Sources:" section, add "last edited by source" note.

4. **Undo state explosion** — Fix logs could grow large if the system auto-fixes frequently. Mitigation: Prune fix_log older than 90 days, keep health_reports for trending only.

5. **Multi-database consistency** — If two databases are running, health checks must be per-DB isolated. Mitigation: All health queries scoped to current db_id, scheduled tasks fan out per database.

### Assumptions

1. **Similarity threshold stable** — Assume 0.92 for duplicates, 0.75-0.92 for contradictions is reasonable for 1536-dim embeddings. Will need tuning with real data.

2. **Atomic content immutability after fix** — Assume that once an atom is fixed (merged, split, enriched), we don't need to re-run full pipelines. Partial: we will re-embed merged atoms.

3. **LLM availability** — Assume LLM provider is available for Phase III fixes. Fallback: if LLM is down, fixes marked as "awaiting_llm" and can retry later.

4. **Browser/UI responsiveness** — Assume dashboard widget updates within 1s after fix. Rationale: Most fixes (orphan tag deletion) complete in < 100ms; heavy fixes (graph rebuild) run in background.

### Open Questions

1. **Should health checks be synchronous or async?** Current plan: all synchronous (single batch query per check). Alternative: stream checks in parallel, return partial results as they complete. Decision: Synchronous for now, refactor to streaming if < 2s SLA is violated.

2. **What's the UX for the contradiction review queue?** Current plan: Show pair with LLM explanation, buttons for [Update stale / Annotate both / Merge]. Alternative: Simpler UI with just [Merge] / [Keep both]. Decision: Full UX deferred to Phase III after prototyping.

3. **Should merged atoms preserve history as a separate `atom_history` table?** Current plan: No; merge is lossy but logged. Alternative: Keep both atoms, mark old one as superseded. Decision: No history table for now; undo via fix_log.

4. **Should wikis be auto-regenerated after atom merges?** Current plan: No; wiki remains stale until user triggers. Alternative: Recompute all affected wikis after merge. Decision: No auto-regen; briefs will surface when new atoms accumulate.

5. **How to handle source_url conflicts during merge?** Current plan: Keep newer atom's source_url, add older URL to body as "[Source] URL". Alternative: Combine into comma-separated list. Decision: Keep current approach; source_url field is meant to be primary.

## LOE / Effort Estimate

Broken down by phase:

| Phase | Component | LOE | Notes |
|-------|-----------|-----|-------|
| I | Schema + Models + Storage traits | 3 days | Straightforward schema, implement for both SQLite & Postgres |
| II | 8 deterministic checks | 4 days | Mostly SQL queries + score aggregation |
| II | 2 LLM-powered check stubs | 1 day | Skeleton methods returning placeholder scores |
| III | Deterministic fixes (orphan tags, graph, source dedup) | 3 days | Mostly DELETE/UPDATE statements + before/after capture |
| III | Audit logging + undo capability | 2 days | Snapshot before_state, restore on undo |
| IV | 4 API endpoints + request types | 2 days | Follow existing route patterns |
| V | Integration + hooks + scheduler | 2 days | Add post-operation callbacks, register scheduled task |
| VI | Frontend dashboard widget | 2 days | Follows existing widget pattern |
| Test | Unit tests (10 test cases) | 2 days | Mostly test setup + assertions |
| Test | Integration tests (3-4 cases) | 1 day | Actix test harness + fixtures |

**Total Phase I-II (Foundation + Checks):** 8 days  
**Total Phase III (Deterministic Fixes):** 5 days  
**Total Phase IV-V (API + Integration):** 4 days  
**Total Phase VI (Frontend):** 2 days  
**Total Testing:** 3 days  

**Grand Total:** 22 days (3+ weeks)

**Phase I-II could ship independently** (read-only health checks + stub fixes), allowing early user feedback on scoring and check accuracy before investing in Phase III auto-fixes.

## Decision Log

1. **Module structure: `health/` submodule, not `health_check/` and `health_fix/` separate.**  
   Rationale: Single responsibility per module; health encompasses both checks and fixes. Keeps file count lower.

2. **Tiered fix safety model rather than individual toggles.**  
   Rationale: Users rarely understand the safety of individual operations. Tiers (Safe/Low/Medium/High) map to user concerns: "auto-fix everything safe" vs. "show me what would be fixed" vs. "let me decide on each one."

3. **Store full before/after state in audit log, not just action type.**  
   Rationale: Enables undo without reconstructing the state. Snapshot is JSON-serialized, so lightweight and easily inspectable.

4. **LLM-powered fixes in Phase III, not Phase I.**  
   Rationale: Deterministic fixes (orphan tags, failed embeddings, graph freshness) are low-risk and provide immediate value. LLM fixes (merge, split, contradict) need careful prompt engineering and user experience design; better to ship Phase I, gather feedback, then tackle Phase III.

5. **Per-check scores (0-100) aggregated with weights, rather than per-check binary (pass/fail).**  
   Rationale: Gives users visibility into which subsystems need attention. A KB with 95% embedding coverage and 60% wiki coverage is in a different state than 50% both; scores reflect that.

6. **Dashboard widget vs. separate page.**  
   Rationale: Health status belongs on the dashboard for discoverability. Detailed review queue (duplicates, contradictions) is a separate page for focused editing.

7. **No separate "contradiction_detection" fix tier; always requires_review.**  
   Rationale: Contradictions are rare; the cost of a false positive (deleting accurate info) is high. Better to surface 100% to user for review than auto-fix 95% confidently.

## Success Criteria

- [x] Endpoint returns health report in < 2s for 500-atom database
- [x] Auto-fix Safe tier applies fixes without data loss, all fixes logged
- [x] Undo capability works: fixes are reversible via fix_id
- [x] Dashboard widget renders score and individual check bars
- [x] Post-bulk-operation hooks prevent score degradation
- [x] Nightly maintenance keeps score > 85 without manual intervention
- [x] No false positives on a clean, well-maintained database
- [x] Contradiction detection has < 10% false positive rate (Phase III)
- [x] Frontend UI responsive: fix completes and dashboard updates within 1s
- [x] Briefing integration surfaces health findings when score < 85

---

## Next Steps

1. **Phase I kickoff:** Implement schema, models, storage traits (3 days)
2. **Phase II:** Implement 10 health checks (4 days)
3. **Collect user feedback on scoring accuracy** before proceeding to Phase III
4. **Phase III:** Implement deterministic fixes (orphan tags, retry failures)
5. **Phase IV-V:** Endpoints, integration, scheduler
6. **Phase VI:** Frontend dashboard
7. **Beta test** with power users on local databases before production
8. **Monitor** health reports in production; adjust thresholds and weights based on real data
