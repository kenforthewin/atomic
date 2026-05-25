# Plan: Modular Knowledge Quality Signals

## Scope

Build knowledge-quality signals that help users decide where curation, synthesis, cleanup, and connection work will pay off next. These signals should be surfaced through Atomic's existing dashboard and briefing experience rather than through a second dashboard.

This is deliberately **not** a system health dashboard. Atomic should not surface failed embeddings, stuck processing jobs, missing chunks, provider outages, or queue state here. If an atom was ingested but not processed, that is a diagnostics/provider/settings concern, not a knowledge-quality concern. The knowledge-quality feature set starts from the assumption that the system is functioning and asks a different question:

> Which parts of this knowledge base are worth improving?

The first version should be deterministic. Use data Atomic already has: atoms, tags, wiki articles, citations, sources, timestamps, semantic edges, embeddings, search metadata if available, and user-visible activity if already tracked. LLM-assisted judgment can come later, behind explicit module settings.

Explicitly **out of scope for v1**:

- Provider health, embedding failures, stuck queues, or background-job diagnostics
- A claim that every tag needs a wiki
- LLM-first evaluation of quality
- Fully automatic cleanup, merging, or wiki generation
- Global telemetry across users
- A second dashboard that competes with the existing dashboard
- A fixed, one-size-fits-all presentation
- Overall 0-100 "health" scoring
- Default auto-remediation
- User-authored arbitrary SQL, JavaScript, or scripts for custom checks

## Product Direction

Atomic users have different knowledge practices. A researcher, writer, Zettelkasten user, read-it-later collector, and project notebook user will not agree on which quality problems matter. The product should therefore be a **modular signal system**, not a hardcoded dashboard.

A signal is a deterministic observation such as:

- This tag is a strong wiki candidate.
- This existing wiki is worth updating.
- These tags may be duplicates.
- This tag is semantically scattered.
- This atom looks disconnected from the rest of the graph.
- This small concept appears unusually central.

The existing dashboard is the home for these signals. The dashboard already has the right shape: a full-width briefing at the top, followed by modular widgets. Knowledge-quality signals should feed both:

- **Briefing:** a short "worth your attention" section with the highest-value suggestions for the current briefing window.
- **Dashboard widgets:** persistent queues for wiki opportunities, synthesis updates, tag cleanup, and connection work.

The signal system is infrastructure. The user-facing product is an improved dashboard and a more useful briefing.

The key product behavior is explainability. A row should never say only "37 atoms." It should say why that matters:

> High-value wiki candidate: 18 atoms, 7 distinct sources, cohesive cluster, recent growth, no current wiki.

## Design Principles

1. **Quality, not correctness.** Processing failures belong in diagnostics. Knowledge quality is about usefulness, organization, synthesis, and retrieval.
2. **Signals, not alerts.** These are opportunities to improve the graph, not warnings demanding immediate action.
3. **Deterministic first.** Start with transparent scoring from existing structured data.
4. **Explain every score.** Each signal should include the component reasons that produced it.
5. **User-configurable.** Modules can be enabled, disabled, weighted, dismissed, snoozed, and ignored for specific targets.
6. **Per-database state.** Preferences, dismissals, snoozes, and cached scores belong to the data database unless they are truly global UI preferences.
7. **One dashboard.** Do not create a competing Knowledge Quality dashboard. Extend the existing dashboard and briefing.
8. **No guilt dashboards.** Avoid creating a page that mostly tells users their knowledge base is incomplete. Prefer ranked next actions with clear payoff.

## Signal Model

Introduce a shared model in `atomic-core` that every provider emits:

```rust
pub struct KnowledgeSignal {
    pub id: String,
    pub provider_id: String,
    pub target: KnowledgeSignalTarget,
    pub score: f32,
    pub confidence: f32,
    pub severity: KnowledgeSignalSeverity,
    pub title: String,
    pub summary: String,
    pub reasons: Vec<KnowledgeSignalReason>,
    pub evidence: serde_json::Value,
    pub suggested_actions: Vec<KnowledgeSignalAction>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

pub enum KnowledgeSignalTarget {
    Atom(String),
    Tag(String),
    Wiki(String),
    Cluster(String),
    Database,
}

pub struct KnowledgeSignalReason {
    pub kind: String,
    pub label: String,
    pub value: serde_json::Value,
    pub contribution: f32,
}
```

`score` ranks opportunity value. `confidence` estimates whether the signal is likely meaningful. A high-score, low-confidence signal can still be worth showing in a "review" queue, but should not dominate the main view.

`evidence` is provider-specific structured data for drilldowns and actions. Reasons are the compact explanation shown in lists; evidence is the richer payload a detail view needs. The API keeps this as JSON, but each provider should define a typed internal evidence struct and serialize that struct into `evidence`.

Evidence payloads should include:

- `schema`: stable provider evidence schema id, such as `wiki_candidate`
- `schema_version`: integer version, starting at `1`

Examples:

- duplicate tag pairs
- atom IDs and titles
- source URLs/domains
- stale wiki metadata
- broken link targets and candidate replacements
- content overlap pairs
- citation counts

Keep `evidence` structured. Avoid forcing every provider to flatten rich review data into reason strings. Avoid ad hoc `json!({...})` evidence in providers; use typed evidence structs so the next signal can copy the pattern.

Use stable signal keys so dismissals survive recomputation. Example:

```text
wiki_candidate:tag:{tag_id}
duplicate_tags:{canonical_sorted_tag_ids_hash}
isolated_atom:atom:{atom_id}
```

## Provider Model

Each module is a `KnowledgeSignalProvider`.

```rust
pub trait KnowledgeSignalProvider {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn default_config(&self) -> KnowledgeSignalProviderConfig;

    async fn evaluate(
        &self,
        core: &AtomicCore,
        config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>>;
}
```

Providers should be independent and composable. The UI should not know whether a signal came from wiki scoring, tag cleanup, graph topology, citation analysis, or search gaps.

Provider config should include:

- `enabled`
- `weight`
- `min_score`
- `min_confidence`
- `show_on_dashboard`
- `include_in_briefing`
- Provider-specific thresholds

User feedback state should be separate from provider config:

- dismissed signals
- snoozed signals
- "ignore this signal type for this target"
- "hide this provider"
- "prioritize this provider"

`show_on_dashboard` and `include_in_briefing` should be independent. Some providers are useful as persistent dashboard queues but too noisy for a recurring briefing. Others are naturally briefing-shaped because they explain what changed recently.

Examples:

- Briefing-friendly: recent growth, new high-value wiki candidates, wikis worth updating, emerging concepts, newly connected clusters.
- Dashboard-first: tag redundancy, empty tags, broad/scattered tags, long-running cleanup queues.

## V1 Modules

### 1. Wiki Candidates

Find tags where generating a wiki is likely to be valuable. This replaces the current simple atom-count style suggestion with a richer deterministic score.

Useful signals:

- Atom count with diminishing returns
- Distinct source URLs/domains
- Number of substantive atoms, excluding tiny notes
- Semantic cohesion among atoms under the tag
- Semantic breadth: enough subclusters to synthesize, but not so scattered that the wiki would be incoherent
- Recent growth
- Cross-link potential with neighboring tags
- Whether a wiki already exists

Initial score shape:

```text
score =
  0.20 * atom_volume
+ 0.20 * source_diversity
+ 0.20 * semantic_cohesion
+ 0.15 * semantic_breadth
+ 0.15 * recent_growth
+ 0.10 * cross_link_potential
```

Reasons should expose the components:

- `22 atoms`
- `9 distinct sources`
- `high semantic cohesion`
- `3 emerging subtopics`
- `8 atoms added in the last 14 days`
- `no current wiki`

This module must not imply every tag without a wiki is a problem.

### 2. Wikis Worth Updating

Find existing wikis where regeneration or proposal creation is likely to improve the article.

Useful signals:

- New atoms since the wiki was last updated
- New atoms with high source diversity
- New atoms semantically far from the current citation set
- New atoms that form a coherent addition rather than one-off noise
- Recent user activity around the tag, if tracked
- Wiki citation set is narrow compared to available atoms

Do not rank by new atom count alone. A single highly novel source may matter more than ten near-duplicates.

Suggested actions:

- generate update proposal
- review new supporting atoms
- open wiki

### 3. Tag Redundancy And Cleanup

Find places where the tag system is becoming redundant, noisy, or misaligned with how atoms are actually organized.

Name similarity is useful, but it should not be the primary signal. Two tags can be operationally redundant even when their names differ, and two similarly named tags can be intentionally distinct. The stronger signal is how the tags are used.

Core redundancy signals:

- **Atom overlap:** tags share a large fraction of the same atoms.
- **Semantic overlap:** the atoms under each tag occupy a similar embedding region.
- **Name similarity:** tag names are lexically similar or synonymous.
- **Hierarchy context:** sibling redundancy is more suspicious than parent/child overlap.
- **Size confidence:** overlap between substantial tags is more meaningful than overlap between tiny tags.

Use both Jaccard and containment because they capture different cases:

```text
jaccard(X, Y) = |atoms(X) intersection atoms(Y)| / |atoms(X) union atoms(Y)|
containment(X, Y) = |atoms(X) intersection atoms(Y)| / min(|atoms(X)|, |atoms(Y)|)
```

Signal types:

- **Possible duplicate tags:** high Jaccard, high semantic similarity, usually sibling or unrelated branches. Example: two 40-atom tags share 36 atoms and have similar centroids.
- **Possible subsumed tag:** high containment but low Jaccard. Example: a 12-atom tag shares 11 atoms with a 100-atom tag. This may indicate a legitimate narrower concept, or a tag that should be reparented/renamed.
- **Scattered tag:** low internal semantic cohesion. The tag may be too broad, overloaded, or inconsistently applied.
- **Empty or low-value tag:** zero atoms and no children, or repeated single-atom tags where the pattern looks accidental.
- **Tag mismatch:** an atom's assigned tags disagree with its semantic neighborhood.

Initial scoring inputs:

- atom count for both tags
- shared atom count
- Jaccard overlap
- containment overlap
- tag centroid similarity
- semantic edge overlap between the two tag scopes
- name similarity
- whether tags are siblings, parent/child, or in unrelated branches
- whether either tag is an auto-tag target or structural category

Guardrails:

- Ignore tiny tag pairs unless the name match is very strong or overlap is exact.
- Treat parent/child containment as lower severity by default.
- Raise thresholds across top-level categories such as `People` vs `Topics`.
- Do not imply that containment always means merge. Often the right action is reparent, rename, or keep.
- Prefer "review redundancy" language over "duplicate" unless evidence is very strong.

Suggested actions:

- review overlapping atoms
- merge tags
- reparent narrower tag
- rename tag
- split broad tag
- prune empty tag
- ignore this pair

V1 should start conservative: high-confidence sibling redundancy, empty tags, and clear near-duplicates. Broad/scattered tags and subsumption should have higher thresholds and more cautious wording.

Actionability matters more than detection volume. A tag-cleanup signal should not ship as a row that only opens a tag and leaves the user to infer the next step. Every organization signal needs a concrete resolution path:

- **Review evidence:** show exactly why Atomic thinks the tags overlap.
- **Choose an outcome:** merge, delete, keep separate, or ignore this pair. Rename and move can be added later if those outcomes prove common enough.
- **Preview impact:** show affected atoms, child tags, wiki articles, and tag assignments before mutation.
- **Apply deliberately:** use existing tag mutation APIs where possible, and add explicit merge/apply routes instead of reusing the LLM-driven compact-tags utility.
- **Record resolution:** dismiss/ignore signals durably so handled pairs do not reappear unchanged.

For the first shippable slice, `review_overlap`, `keep_separate`, `merge_tags`, and `delete_empty_tag` are the useful outcomes. Merge is the primary action for true duplicate-tag signals, but it must be exposed through a deterministic preview and confirmation flow. The existing internal merge operation can be reused behind a new explicit action route, but the current one-click `compact_tags` route should not be the UI action for a deterministic signal because it lets an LLM choose and apply merges in one step.

Initial implementation should split this module into shippable providers rather than trying to solve all tag quality problems at once:

- `tag_redundancy`: high-confidence possible duplicate or subsumed tag pairs.
- `empty_tag`: zero-atom, childless tags that are safe to review as cleanup opportunities.
- Later: `scattered_tag`, `tag_mismatch`, and richer semantic-cohesion providers once the cohesion metric is better defined.

`tag_redundancy` should use stable pair keys:

```text
tag_redundancy:pair:{canonical_sorted_tag_ids_hash}
```

The signal target can be the primary tag while the evidence carries both tags. This avoids inventing a new public target type before the UI needs one.

Typed evidence should include:

- both tag IDs, names, parent IDs, paths, and atom counts
- shared atom count
- Jaccard overlap
- containment overlap
- centroid similarity when both tags have embeddings
- semantic edge overlap when cheap to compute
- name similarity
- hierarchy relationship: sibling, parent-child, ancestor-descendant, unrelated, or cross-category
- whether either tag is an auto-tag target or structural category
- merge-impact counts for the review UI, including child tags and whether the removed tag has a wiki
- recommended review posture: possible duplicate, possible subsumption, or review overlap

Default thresholds should be intentionally conservative:

- Ignore pairs where both tags have fewer than 5 atoms unless overlap is exact and names are highly similar.
- Prefer duplicate signals when Jaccard is very high and centroid/name/hierarchy evidence agrees.
- Prefer subsumption signals when containment is high but Jaccard is much lower.
- Suppress or strongly down-rank expected parent-child overlap.
- Suppress cross-category pairs unless the evidence is overwhelming.
- Keep empty-tag signals dashboard-only by default.

### 4. Concepts To Strengthen

Find areas that look important but underdeveloped.

Useful signals:

- Small tags with high graph centrality
- Thin tags adjacent to large, active clusters
- Frequently referenced concepts with few atoms, if search/chat/activity history exists
- Wiki articles with very few citations despite a broad topic
- Clusters that appear coherent but lack a representative tag

Suggested actions:

- add notes
- review related atoms
- create tag
- generate seed wiki

This module may be less precise in v1. Keep it opt-in or lower priority until the deterministic heuristics prove useful.

### 5. Ideas To Connect

Find atoms or small groups that appear disconnected in a knowledge-quality sense, not a processing-health sense.

Useful signals:

- Atom has few semantic edges
- Atom has no tag overlap with nearby atoms
- Atom is close to a tag cluster but not tagged into it
- Atom is never cited in a wiki where nearby atoms are cited
- Atom appears semantically related to a concept but sits outside the user's organization

Suggested actions:

- review connections
- suggest tags
- add to existing wiki scope
- open related atoms

This should avoid shaming legitimate standalone notes. Isolation is only interesting when there is evidence the atom belongs near something else.

## Later Modules

These modules are not part of the first slice, but PR #182 showed that they can be valuable when framed as knowledge-quality signals rather than health checks.

### Link Quality

Find broken or weak internal links inside atom content.

Useful signals:

- `[[wikilink]]` targets that do not resolve to an atom
- markdown links to local note paths that no longer resolve
- links that likely point to an existing atom under a renamed title/path
- repeated broken links from the same imported source

Suggested actions:

- relink to suggested atom
- remove link
- ignore this link
- ignore broken links in this atom

This belongs in dashboard drilldowns more than briefings. Only high-confidence, newly introduced link-quality issues should appear in briefing suggestions.

### Source Pollution And Boilerplate

Find content patterns that make retrieval, similarity, and synthesis worse.

Useful signals:

- repeated boilerplate chunks across many atoms
- near-duplicate source content
- atoms dominated by template text
- content overlap pairs that are likely duplicate captures
- source URL duplicates

Suggested actions:

- review duplicate captures
- strip boilerplate proposal
- merge or archive duplicate content
- ignore repeated template for this source

This provider should be careful. Boilerplate detection can be deterministic, but rewriting content should be a reviewed proposal with undo, not an automatic fix.

### Custom Signal Providers

Users should eventually be able to define their own deterministic rules. This is important because knowledge quality is subjective: one user's invariant is another user's noise.

Custom rules should be declarative and bounded. No arbitrary SQL, JavaScript, shell commands, or user-authored scripts. Atomic should expose a small set of safe rule shapes:

- **Require source:** atoms under a tag must have a source URL.
- **Require tag:** atoms in a scope must carry at least one tag from a set.
- **Forbidden tag combination:** atoms must not carry mutually exclusive tags.
- **Tag requires tag:** atoms with any tag in one set must also have required tags.
- **Tag cardinality:** atoms in a scope should have min/max tag counts.
- **Content length:** atoms in a scope should stay within word/character bounds.
- **Missing heading:** longer atoms should contain markdown headings.
- **Source domain allow/block list:** atoms in a scope should or should not come from specific domains.
- **Stale atom by tag:** atoms under a tag should be reviewed after a configurable age.
- **Content regex:** later, with strict pattern and runtime limits.

Custom rules should use templates first:

- "Research notes should have sources."
- "Project notes should have a status tag."
- "Published notes cannot also be drafts."
- "Book notes should include an author/source."
- "Meeting notes older than 30 days should be reviewed."

The UX should include preview before enabling: show how many atoms would be flagged and sample affected atoms. Custom rules should emit normal `KnowledgeSignal`s so they inherit dashboard display, briefing eligibility, dismiss/snooze, and action handling.

Sequence this after built-in providers, feedback state, drilldowns, and action audit are stable.

## Profiles

Profiles are presets over module enablement, weights, and thresholds. They are not separate implementations.

Potential presets:

- **Researcher:** wiki candidates, citation richness, source diversity, stale synthesis
- **Writer:** mature topics, thin arguments, high-value wikis, source-rich clusters
- **PKM / Zettelkasten:** isolated atoms, weak connections, emerging concepts, tag cleanup
- **Collector:** duplicate sources, source clusters, wiki candidates, unread or unsynthesized areas
- **Minimal:** only high-confidence, high-score suggestions

Profiles should be optional. Users can always tune modules directly.

## UI Shape

The first UI should extend the existing dashboard, not introduce a new destination. The current dashboard is already organized as a full-width `BriefingWidget` followed by modular widgets in `src/components/dashboard/registry.ts`. Knowledge-quality work should use that structure.

### Briefing Integration

The briefing is the most important surface. It should continue to summarize recent knowledge, but it should also include a structured "Worth your attention" section selected by deterministic signals.

The LLM should not invent or rank these suggestions. The deterministic signal system should choose the items, then the briefing renderer can display them as structured companions to the generated prose.

Example:

```text
Worth your attention

- Generate a wiki for Distributed Systems: cohesive cluster, 12 atoms, 5 sources, recent growth.
- Update LLM Evaluation: 4 new sources materially expand the existing article.
- Review possible duplicate tags: AI Agents and Agentic AI.
```

This section should have its own citations or target links where appropriate, but it does not need to be part of the model-generated briefing body. Keeping it structured preserves trust: users can see the deterministic reasons and act directly.

### Dashboard Widgets

Signals should also appear as persistent dashboard widgets. Initial widgets:

- **Best Wikis to Generate:** replaces or upgrades `NewWikisWidget`, ranking by signal score instead of atom count.
- **Wikis Worth Updating:** existing articles where new material is likely to improve synthesis.
- **Tag Cleanup:** redundant, subsumed, empty, broad, or semantically scattered tags.
- **Ideas To Connect:** isolated atoms or missing tag overlap with nearby clusters.

The dashboard should not show every enabled signal in one large inbox by default. Widgets should be compact queues, with a drilldown available when the user needs detail.

Each row should include:

- target name and type
- score/confidence presentation
- compact reason chips
- suggested action buttons
- dismiss/snooze menu
- "hide this type" affordance

Rows should open into a detail view showing the exact deterministic reason breakdown. Users should be able to tell why Atomic thinks something is useful without trusting a black box.

### Optional Detail View

A deeper "all opportunities" view may still be useful, but it should be a drilldown from dashboard widgets, not the primary feature. It can provide:

- merged ranked list across enabled modules
- module filters
- dismissed/snoozed recovery
- provider configuration

This preserves one dashboard while still giving power users a place to inspect and tune the signal system.

### Drilldown Interaction Patterns

PR #182's review queue has several interaction patterns worth carrying forward into signal drilldowns:

- Per-module filters for severity, confidence, actionability, and sort order.
- Per-module re-scan buttons that refresh one provider without recomputing everything.
- Inline actions for simple fixes like ignore, relink, open atom, open tag, or generate wiki.
- Bulk selection for dismissing or reviewing many low-risk items.
- Virtualized lists for long queues.
- Markdown export for a module's current queue.
- "Resolved today" or "handled this session" counters scoped to the active database.

These should live inside widget drilldowns or modals. The default dashboard should stay compact.

## Storage

Use per-database storage for knowledge-quality state:

```sql
CREATE TABLE knowledge_signal_preferences (
    provider_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL,
    weight REAL NOT NULL,
    min_score REAL NOT NULL,
    min_confidence REAL NOT NULL,
    show_on_dashboard INTEGER NOT NULL,
    include_in_briefing INTEGER NOT NULL,
    config_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE knowledge_signal_feedback (
    signal_key TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT,
    state TEXT NOT NULL, -- dismissed | snoozed | ignored
    snoozed_until TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE knowledge_signal_cache (
    signal_key TEXT PRIMARY KEY,
    provider_id TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT,
    score REAL NOT NULL,
    confidence REAL NOT NULL,
    payload_json TEXT NOT NULL,
    computed_at TEXT NOT NULL,
    expires_at TEXT
);

CREATE TABLE knowledge_signal_action_log (
    id TEXT PRIMARY KEY,
    signal_key TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT,
    before_state_json TEXT,
    after_state_json TEXT,
    executed_at TEXT NOT NULL,
    undone_at TEXT
);

```

The cache can be skipped initially if on-demand computation is fast enough, but feedback state should exist from the beginning. Dismiss/snooze is part of making the modular system tolerable.

The action log is only needed once mutating signal actions ship. It should not block read-only v1. When implemented, use it for actions that modify atoms, tags, wiki content, or links. Non-mutating actions such as opening a tag or generating a preview do not need audit rows.

Because this is per-database state, helpers should write through `core.storage()` rather than `AtomicCore::set_setting()` when a registry is attached.

Postgres uses the same logical schema with `db_id` in the primary keys because all logical databases share the same physical tables.

## Actions And Undo

Signals can suggest actions, but v1 should be conservative:

- Read-only actions: open atom, open tag, open wiki, review related atoms.
- Proposal actions: generate wiki proposal, strip boilerplate proposal, tag merge proposal.
- Direct mutating actions: only when the operation is narrow, explicit, and undoable.

Before any direct mutation ships, add an action audit path:

- capture before/after state for touched atoms/tags/wiki/link content
- write an action log row
- expose undo for supported actions
- show a short undo affordance in the UI when practical

This borrows the useful part of PR #182's fix audit system without adopting automatic remediation as a product default.

### Action Audit Foundation

Signal-driven mutations should go through a dedicated action endpoint rather than through the general app mutation routes:

```text
POST /api/knowledge-signals/{signal_key}/actions
POST /api/knowledge-signals/actions/{action_log_id}/undo
```

This keeps audit and undo semantics scoped to knowledge-quality actions. Normal tag, atom, and wiki operations should remain normal user actions and should not be retroactively treated as signal remediations.

The first action-log slice should support:

- `add_tag_to_atom`: audited and undoable.
- `delete_empty_tag`: audited, not undoable yet.
- `merge_tags`: audited, not undoable yet.

Undo support is action-specific. The initial undo contract only covers `add_tag_to_atom`: undo removes the tag assignment only when the signal action actually created that assignment. Tag deletion and tag merge need more complete restoration semantics before undo should be exposed.

Successful signal actions should dismiss the originating signal so completed work does not immediately reappear.

## Computation Strategy

Start with manual/on-demand evaluation:

1. User opens the dashboard.
2. Server evaluates enabled providers for the active database.
3. Results are normalized, filtered by feedback state, ranked, and returned to dashboard widgets.

The dashboard's briefing surface should use the same signal infrastructure, but not the retired briefing storage path:

1. The featured-report widget fetches its latest report finding normally.
2. The widget also requests briefing-eligible signals from the deterministic provider layer.
3. The UI renders the top few signals beside or below the generated report finding content.
4. Dismissal still writes to `knowledge_signal_feedback`, so refreshes and navigation do not resurrect dismissed suggestions.

Do not make the LLM responsible for deciding which knowledge-quality suggestions to show. It can summarize recent atoms; deterministic providers should pick the actions.

Add caching before broad user rollout. Live evaluation is useful during initial
development, but dashboard load should not repeatedly recompute every visible
provider for users with large databases. A simple cache invalidation model is
enough:

- atom created/updated/deleted
- atom tags changed
- tag renamed/merged/deleted
- wiki generated/updated
- semantic edges rebuilt

For early development, it is acceptable to recompute deterministic providers
for the active database when the dashboard opens. Before releasing this broadly,
dashboard evaluation should move to a bounded, cache-aware path. Historical
report findings do not snapshot these suggestions in the first implementation;
signals are current maintenance prompts, not part of generated report content.

## API Surface

Core:

- `AtomicCore::list_knowledge_signals(filter)`
- `AtomicCore::list_briefing_knowledge_signals(window, limit)` for the dashboard's briefing surface
- `AtomicCore::set_knowledge_signal_provider_config(provider_id, config)`
- `AtomicCore::dismiss_knowledge_signal(signal_key)`
- `AtomicCore::snooze_knowledge_signal(signal_key, until)`
- `AtomicCore::restore_knowledge_signal(signal_key)`
- `AtomicCore::apply_knowledge_signal_action(signal_key, action)`
- `AtomicCore::undo_knowledge_signal_action(action_log_id)` for actions with defined undo semantics
- Later: `AtomicCore::preview_custom_signal_rule(rule)`

Server:

- `GET /api/knowledge-signals`
- `GET /api/knowledge-signals?surface=briefing`
- `PUT /api/knowledge-signals/providers/:provider_id`
- `POST /api/knowledge-signals/:signal_key/dismiss`
- `POST /api/knowledge-signals/:signal_key/snooze`
- `POST /api/knowledge-signals/:signal_key/restore`
- `POST /api/knowledge-signals/:signal_key/actions`
- `POST /api/knowledge-signals/actions/:action_log_id/undo`
- Later: `POST /api/knowledge-signals/custom/preview`

Frontend transport should expose these as normal commands. The React page should stay transport-agnostic.

## Milestones

Milestones 1-3 now establish the baseline pattern: typed deterministic providers, per-database preferences and feedback, SQLite/Postgres support, dashboard widgets, and live briefing-surface suggestions. Future milestones should preserve that pattern: each phase should be independently shippable, with conservative defaults and durable dismissals.

### Milestone 1 - Signal Foundation

- Add shared signal models in `atomic-core`.
- Add provider registry with deterministic providers.
- Add provider config and feedback storage.
- Add server routes and transport commands.
- Implement dismiss, snooze, restore, and provider enable/disable.
- Build the first dashboard widget backed by signals.
- Upgrade `NewWikisWidget` to use `WikiCandidateProvider` ranking rather than atom count.
- Support both SQLite and Postgres for provider evaluation, preferences, and feedback state.

Exit criteria: the existing dashboard can show deterministic signals from at least one provider, explain them, and remember when the user dismisses or snoozes them on either storage backend. There is no new dashboard route.

### Milestone 2 - Briefing Surface Suggestions

- Add `include_in_briefing` provider config.
- Evaluate briefing-eligible signals on demand for the reports-backed `BriefingWidget`.
- Render a "Worth your attention" section below the latest featured report finding.
- Keep deterministic suggestions separate from the LLM-generated briefing prose.

Exit criteria: the dashboard briefing surface can include 3-5 actionable knowledge-quality suggestions, each with deterministic reasons and direct actions. Dismissed suggestions stay dismissed across refresh/navigation via signal feedback.

### Milestone 3 - Better Wiki Opportunities

- Replace or augment the current wiki suggestions with `WikiCandidateProvider`.
- Score wiki candidates using atom volume, source diversity, cohesion, breadth, recency, and wiki absence.
- Add `WikiUpdateProvider` for existing articles worth updating.
- Wire suggested actions to existing wiki generation/update flows.

Exit criteria: wiki suggestions are no longer ranked only by literal atom count, and each suggestion explains why it is worth generating or updating.

### Milestone 4 - Organization Signals

- Add conservative tag-redundancy detection using atom overlap, hierarchy context, centroid similarity where available, and name similarity.
- Add empty-tag cleanup signals for zero-atom, childless tags.
- Add a compact dashboard widget for tag cleanup opportunities.
- Make only the highest-confidence redundancy signals briefing-eligible.
- Defer scattered-tag and tag-mismatch signals until semantic cohesion is defined beyond centroid similarity.
- Add UI actions for opening tags and reviewing overlapping atoms. Merge, reparent, rename, and prune actions can appear as suggested actions only when the corresponding flow exists.

Exit criteria: users can identify and dismiss high-confidence tag redundancy and cleanup opportunities without the system making subjective cleanup decisions automatically.

#### Milestone 4A - Tag Redundancy Provider

Implement `TagRedundancyProvider` as the first organization provider.

Provider behavior:

- Generate candidate tag pairs from shared atom membership rather than all-pairs tag comparison.
- Compute Jaccard and containment for each pair.
- Add hierarchy context: sibling, parent-child, ancestor-descendant, unrelated, or cross-category.
- Add centroid similarity when both tags have centroids. If centroid data is unavailable, fall back to atom-overlap and hierarchy evidence rather than blocking the signal.
- Add name similarity as supporting evidence, not as the main signal.
- Return stable signal keys using canonical sorted tag IDs.
- Emit typed evidence with both tag records, overlap statistics, semantic statistics, hierarchy relationship, and merge-impact fields.

Initial conservative thresholds:

- Candidate generation requires at least 3 shared atoms.
- Duplicate-style signal requires Jaccard `>= 0.80`, or Jaccard `>= 0.65` plus strong name similarity and sibling/unrelated hierarchy.
- Subsumption-style signal requires containment `>= 0.85`, the smaller tag has at least 5 atoms, and the relationship is not an obvious intentional parent-child scope.
- Cross-category pairs require stricter thresholds and should usually be excluded from briefing.
- Pairs involving structural category roots should be suppressed.

Suggested reasons:

- `36 shared atoms`
- `90% overlap`
- `similar tag scopes`
- `sibling tags`
- `one tag mostly contained in the other`

Suggested actions:

- `review_overlap`: open a review modal showing overlap metrics and atom counts unique to each tag.
- `keep_separate`: durably ignore this pair unless the overlap evidence materially changes.
- `open_primary_tag`
- `open_secondary_tag`
- `merge_tags`: merge one selected source tag into one selected target tag after preview and confirmation.

#### Milestone 4B - Empty Tag Provider

Implement `EmptyTagProvider` as a small dashboard-first cleanup provider.

Provider behavior:

- Identify tags with zero atoms and zero children.
- Suppress structural category roots and provider/system categories.
- Use stable keys: `empty_tag:tag:{tag_id}`.
- Keep `include_in_briefing` disabled by default.

Suggested reasons:

- `0 atoms`
- `no child tags`

Suggested actions:

- `open_tag`
- `delete_empty_tag`, with confirmation. Empty childless tags have no atom assignments or descendants, so this can be the first direct cleanup action.

The empty-tag row should show the destructive action plainly. It should not require the user to open the tag and find a separate delete button.

#### Milestone 4C - Dashboard And Review UI

Add a `Tag Cleanup` dashboard widget backed by organization signals.

Initial widget behavior:

- Show the top 5 tag cleanup signals.
- Group or label rows by signal type: possible duplicate, possible subsumption, empty tag.
- Render the strongest 1-2 reasons inline.
- Support durable dismiss using existing knowledge-signal feedback.
- Clicking a redundancy row should open a modal with:
  - both tag names and hierarchy paths
  - shared atom count and overlap metrics
  - atom counts unique to each tag
  - merge-impact counts, including atom assignments added, child tags moved, and whether a wiki would be deleted
  - persistent action buttons in a bottom modal action bar
  - suggested next action language that avoids implying automatic merge

The review modal is part of the milestone, not a polish follow-up. Without it, redundancy signals are not actionable enough.

Review view outcomes:

- **Keep Separate:** records durable feedback for this pair and removes the signal.
- **Merge:** requires choosing the tag to keep, then shows a confirmation before applying. The preview must show how many atom assignments will be added, child tags that will move, whether the removed tag has a wiki article, and the target tag that remains.
- **Rename:** later, if a name cleanup action proves common enough to need an inline flow.
- **Move:** later, if containment suggests hierarchy cleanup often enough to need a parent selector.
- **Open Tags:** opens either tag in the normal tag-filtered view for manual inspection.

Empty-tag outcomes:

- **Delete:** confirms and applies through `delete_tag` with `recursive=false`.
- **Keep:** dismisses or ignores the signal.

#### Milestone 4D - Briefing Eligibility

Only high-confidence redundancy signals should appear in briefings by default.

Briefing inclusion rules:

- Include duplicate-style tag redundancy when confidence is high and the pair is not cross-category.
- Exclude empty tags.
- Exclude low-confidence subsumption unless the score is very high and the hierarchy context is clear.
- Cap organization signals so they do not crowd out wiki suggestions.

#### Milestone 4E - Tests And Storage Parity

The milestone is not shippable until both storage backends and feedback paths are covered.

Core tests:

- duplicate-like sibling tags produce a `tag_redundancy` signal with typed evidence
- high containment produces a subsumption-style signal, not a duplicate-style title
- parent-child overlap is down-ranked or suppressed
- structural category roots are suppressed
- dismissed redundancy signals are hidden unless dismissed signals are requested
- provider dashboard/briefing preferences are honored

Storage checks:

- SQLite provider evaluation works against normal tag/atom fixtures.
- Postgres provider evaluation uses the same evidence shape and purge behavior.
- No new per-database state routes through registry settings.

Frontend checks:

- `Tag Cleanup` widget renders redundancy and empty-tag rows.
- dismiss removes a row and persists through reload.
- empty-state copy does not frame cleanup as required maintenance.

### Milestone 5 - Connection And Strength Signals

- Add isolated atom signals that require evidence of nearby clusters.
- Add "missing tag overlap" signals for atoms close to a tag cluster.
- Add thin-but-central concept signals.
- Consider optional use of search/chat history if local metadata exists and the user has enabled the module.

Exit criteria: Atomic can surface underconnected or underdeveloped knowledge areas without confusing them with processing failures.

#### Milestone 5A - Missing Tag Overlap

Implement `MissingTagOverlapProvider` as the first connection provider.

Provider behavior:

- Use existing semantic edges rather than a new clustering job.
- For each atom, inspect nearby atoms above a conservative similarity threshold.
- Count tags that repeatedly appear on nearby atoms but are missing from the target atom.
- Suppress suggestions where the atom already has the tag or a direct ancestor/descendant tag.
- Suppress structural category roots and tiny candidate tags unless similarity is very high.
- Keep this dashboard-first by default; briefing eligibility can come later for very high-confidence cases.

Typed evidence should include:

- atom ID and title
- current tag count
- suggested tag record, including path, atom count, child count, and wiki presence
- nearby tagged atom count
- strongest similarity
- average similarity

Suggested actions:

- `add_tag_to_atom`: apply the existing tag to the atom without rerunning the embedding/tagging pipeline.
- `open_atom`
- `dismiss`

This action is low risk enough to ship before undo infrastructure because it only adds a normal manual tag assignment. Durable dismiss still applies so accepted or rejected suggestions do not immediately reappear.

#### Milestone 5B - Underconnected Atoms

Implement `UnderconnectedAtomProvider` as a conservative dashboard-first review signal.

Provider behavior:

- Consider captured atoms only.
- Require `edges_status = complete` so unprocessed atoms are not surfaced as knowledge-quality issues.
- Require a minimum active database size before emitting signals, since isolated notes in tiny databases are expected.
- Suppress tiny notes where lack of connection is not meaningful.
- Rank atoms with few or no strong semantic edges, low closest-neighbor similarity, and sparse tag context.
- Keep briefing eligibility disabled by default.

Typed evidence should include:

- atom ID, title, source URL, and content length
- tag count
- total semantic edge count
- strong semantic edge count
- strongest and average similarity when available
- captured atom count for the active database
- edge status used to qualify the signal

Suggested actions:

- `open_atom`
- `dismiss`

This provider should not imply that every standalone note is wrong. It should be framed as a review opportunity for atoms that may be misfiled, obsolete, or worth connecting through tags or follow-up notes.

### Milestone 6 - Link And Source Quality

- Add near-duplicate atom detection as a pulled-forward deterministic cleanup provider.
- Add broken internal link detection as a dashboard-first provider.
- Add source URL duplicate and content-overlap providers.
- Add boilerplate/source-pollution detection where the signal can be deterministic.
- Add reviewed proposal flow for any content rewrite or boilerplate stripping action.
- Add audit/undo infrastructure before any direct mutating action ships.
- After source duplicates and broken internal links, pause new signal expansion and shift effort to action quality, review flows, undo coverage, and dashboard polish.

Exit criteria: Atomic can surface link and source-quality issues with concrete evidence and safe review actions, without treating them as system health failures or auto-fixing content by default.

#### Milestone 6A - Near-Duplicate Atoms

Implement `NearDuplicateAtomProvider` as the first content-cleanup provider.

Provider behavior:

- Use existing semantic edges for candidate generation rather than all-pairs atom comparison.
- Consider captured atoms only, so report findings and other system atoms do not pollute the queue.
- Score pairs using semantic similarity, exact normalized source match, title similarity, shared tags, and content-length compatibility.
- Require either very high semantic similarity or high semantic similarity plus another supporting signal.
- Use stable keys: `near_duplicate_atom:pair:{canonical_sorted_atom_ids_hash}`.
- Keep briefing eligibility disabled by default.

Typed evidence should include:

- both atom IDs, titles, source URLs, content lengths, and timestamps
- semantic similarity
- source match status
- title similarity
- shared tag names and count
- content-length ratio

Suggested actions:

- `review_pair`
- `open_primary_atom`
- `open_secondary_atom`
- `keep_separate`

This first slice should not merge, archive, or rewrite atom content automatically. Those actions need an audit/undo path first because they mutate the user's actual knowledge content rather than organization metadata.

#### Milestone 6B - Source Duplicates In Similar Notes

Implement `SourceDuplicateProvider` as a deterministic companion to near-duplicate atoms, rendered inside the existing Similar Notes widget rather than as a separate dashboard widget.

Provider behavior:

- Consider captured atoms only.
- Group atoms by normalized exact source URL, ignoring empty or missing sources.
- Score same-source pairs highly, with title similarity, content-length compatibility, and duplicate-group size used only for ranking.
- Avoid all-pairs explosions by pairing the newest capture for a source against a bounded number of older captures.
- Use stable keys: `source_duplicate:pair:{canonical_sorted_atom_ids_hash}`.
- Keep briefing eligibility disabled by default.

Typed evidence should include:

- both atom IDs, titles, source URLs, content lengths, and timestamps
- canonical source URL and normalized source URL
- duplicate count for that source
- title similarity
- content-length ratio

Suggested actions:

- `review_pair`
- `open_primary_atom`
- `open_secondary_atom`
- `keep_separate`

The first UI pass should present this as another kind of "similar note" so users do not have to understand the distinction between semantic overlap and exact source overlap before deciding whether to review the pair.

#### Milestone 6C - Broken Internal Links

Implement `BrokenInternalLinkProvider` as a dashboard-first link quality provider.

Provider behavior:

- Consider links from captured atoms only.
- Surface UUID-shaped atom links whose target atom is missing as the highest-confidence case.
- Surface unresolved text wikilinks more conservatively, because some users intentionally create links to future notes.
- Use stable keys based on the materialized atom link row: `broken_internal_link:link:{link_id}`.
- Bound results per source atom so a single messy note does not dominate the dashboard.
- Keep briefing eligibility disabled by default.

Typed evidence should include:

- link ID
- source atom ID and title
- raw link target and optional link label
- target kind
- link status
- source offsets when available

Suggested actions:

- `open_atom`
- `dismiss`

This provider should not rewrite links or create placeholder atoms in v1. The useful first action is to take the user directly to the source atom with enough context to fix or intentionally keep the link.

#### Milestone 6D - Signal Productization

After the first set of deterministic providers is in place, pause new signal expansion and make the existing feature set feel coherent, configurable, and action-oriented.

This milestone is the bridge between "we can detect useful things" and "users can safely act on useful things." It should finish before profile presets or custom rule providers, because profiles and custom rules will inherit the same action, feedback, and customization patterns.

Workstreams:

- **Surface refinement:** make dashboard widgets and briefing suggestions visually and behaviorally consistent.
- **Action expansion:** turn the most obvious next steps into explicit, audited actions.
- **Customization:** expose provider-level controls in settings so users can tune the system without editing configuration or waiting for custom rules.
- **Undo and audit maturity:** expand undo only where the restoration semantics are well-defined.

Implementation order:

1. Add a provider-settings read API so the frontend can render real per-database provider preferences.
2. Add a Settings → Signals tab for provider enablement, dashboard visibility, briefing visibility, and simple sensitivity controls.
3. Normalize current dashboard widget interaction patterns and loading states.
4. Move wiki generate/update through explicit signal actions with duplicate-job protection.
5. Review tag cleanup and missing-tag actions for consistent audit, success, failure, and undo behavior.
6. Run the audit sweep before adding profile presets or custom providers.

##### 6D.1 - Surface Refinement

Current widgets should follow the same interaction rules:

- Rows should not perform surprising mutations on click.
- Mutating or long-running actions should be explicit buttons with loading/disabled states.
- Dismiss buttons should use consistent labels and icons.
- Empty states should be neutral and not guilt-oriented.
- Each signal widget should have a small help affordance explaining what the signal means and which deterministic evidence drives it.
- Reasons should use user-facing language, not implementation metrics such as "Jaccard" or "containment."
- Review modals should use persistent bottom action bars when the content can scroll.
- Dashboard widgets should use the same compact half-width layout unless a specific workflow needs full width.
- Briefing suggestions should stay capped and should not crowd out the generated briefing content.

Exit criteria: every shipped signal widget has clear action affordances, durable dismiss behavior, loading states for long-running work, and copy that explains the opportunity in product language.

##### 6D.2 - Action Expansion

Prioritize actions that are high-value, explicit, and easy to make safe.

Near-term action candidates:

- `generate_wiki` for wiki candidate signals.
- `update_wiki` for wiki update signals.
- `add_tag_to_atom` for missing-tag overlap, already audited and undoable.
- `delete_empty_tag` for empty-tag cleanup, audited with confirmation.
- `merge_tags` for tag redundancy, audited with preview and confirmation.
- `open_atom` for broken links, underconnected atoms, and similar-note review.
- `keep_separate` / `dismiss` for pairs the user has reviewed.

Duplicate atom resolution needs a dedicated review outcome, not a bare delete button. The final phase should define a conservative `resolve_duplicate_atom` path before any destructive action appears. The first shippable mutation should likely be an archive/remove-one action for high-confidence same-source duplicates only, and only if Atomic has reversible delete/restore semantics. Full atom merge should stay deferred until content, tags, source URLs, links, citations, and undo behavior have a clear preview and restore model.

Defer or treat as proposal-only until the UX and undo model are strong:

- atom merge
- atom archive/delete from duplicate-note signals
- link rewrite or link removal
- placeholder atom creation from unresolved text links
- boilerplate stripping or content rewrite

Implementation expectations:

- Mutating signal actions should go through `POST /api/knowledge-signals/{signal_key}/actions`.
- Successful actions should dismiss the originating signal.
- Actions that can be triggered repeatedly should have client-side and server-side idempotency or in-flight protection.
- Undo should be exposed only when the undo semantics are specific and reliable.
- Every new action should document its before/after state and whether undo is supported.

Exit criteria: the core wiki and tag cleanup suggestions can be completed from the dashboard through explicit actions, and users receive accurate loading, success, failure, and undo feedback.

##### 6D.3 - Provider Customization UI

Expose the existing provider preference model through a settings surface.

Initial controls:

- enable or disable each provider
- show or hide each provider on the dashboard
- include or exclude each provider from the briefing
- choose a simple sensitivity level where possible, mapped to `min_score` and `min_confidence`
- restore dismissed signals or show dismissed signals for review

Keep advanced controls conservative:

- Raw provider weights can remain hidden initially unless the UI has a clear ranking model.
- Provider-specific config should appear only when there is a product-facing concept to configure.
- Profile presets should wait until defaults have been tested against real usage.

The settings surface should describe what each provider does in terms of user value:

- Wiki opportunities
- Wiki updates
- Tag cleanup
- Ideas to connect
- Similar notes
- Broken links
- Underconnected notes

Exit criteria: a user can make the signal system quieter, louder, dashboard-only, or briefing-aware without database edits, and preferences remain per-database.

##### 6D.4 - Review And Audit Sweep

Before moving to profile presets or custom providers, run a review pass across the whole feature set.

Backend checks:

- provider preferences are honored consistently on dashboard and briefing paths
- feedback state filters all widgets and briefing suggestions
- Postgres and SQLite emit the same evidence shape
- per-database state is purged when a logical database is deleted
- mutating actions are audited and only expose undo when implemented

Frontend checks:

- widgets do not duplicate the same opportunity in confusing ways
- action buttons are discoverable and do not rely on row-click side effects
- dismissed or completed signals do not reappear on refresh
- loading and error states are present for every async action
- copy stays away from system-health framing and overall scores

Exit criteria: current deterministic signals feel like one feature set rather than a collection of experimental widgets.

### Milestone 6E - Performance Hardening

Before broad rollout, make signal evaluation safe for real user databases with
tens of thousands of atoms. This milestone should happen after the current
signal/action set stabilizes and before profiles or custom providers, because
every later module will inherit the same evaluation path.

The current development implementation is intentionally direct: dashboard
widgets request their provider data independently, and each provider evaluates
live. That is acceptable while the surface is changing, but it has two rollout
risks:

- dashboard load fans out into several HTTP requests
- `limit` is applied after provider evaluation in many paths, so small widgets
  can still trigger large scans or aggregations

Performance work should preserve the modular provider model while changing how
the dashboard asks for and caches results.

Implemented groundwork:

- Dashboard widgets now load through a single aggregate signal request instead
  of one initial request per widget/provider.
- Hidden or disabled dashboard providers are filtered before their widgets mount.
- The aggregate response is cached briefly per database/core handle and
  invalidated by signal feedback, provider settings, primary atom mutations,
  tag mutations, and wiki mutations.
- The highest-risk live providers now have explicit candidate budgets:
  tag-pair generation, missing-tag overlap, source duplicate groups,
  underconnected atom candidates, and near-duplicate shared-tag loading.

#### 6E.1 - Dashboard Aggregation API

Add one dashboard-oriented read path that evaluates the visible dashboard
providers for the active database and returns grouped results.

Candidate API:

- `AtomicCore::list_dashboard_knowledge_signals(options)`
- `GET /api/knowledge-signals/dashboard`

The response should include:

- provider settings used for filtering
- grouped signal results keyed by provider id
- per-provider timing metadata in debug/development builds
- errors isolated per provider, so one slow or broken provider does not blank the dashboard

The existing provider-specific `GET /api/knowledge-signals?provider_id=...`
route should remain for drilldowns, compatibility, and targeted refreshes. The
dashboard should prefer the aggregate route.

Frontend changes:

- Load provider settings before mounting signal widgets, or have the aggregate
  endpoint decide widget visibility so hidden/disabled widgets never fire their
  provider requests.
- Replace widget-level initial fetches with a shared dashboard signal store or
  context populated by the aggregate endpoint.
- Keep targeted refresh after an action, but refresh only the affected provider
  or signal group when possible.

Exit criteria: opening the dashboard makes one signal request for the dashboard
signal groups, plus targeted follow-up requests only after user actions.

#### 6E.2 - Provider Query Budgets

Each provider needs an explicit cost model and a hard result/candidate budget.
Do not rely on frontend `limit` alone.

Initial provider expectations:

- **Wiki candidates:** aggregate by tag using indexed joins; cap returned
  candidates after scoring. Consider prefiltering to tags with enough atoms or
  recent activity before joining semantic-edge summaries.
- **Wiki updates:** evaluate only existing wiki articles; keep bounded by wiki
  article count rather than atom count where possible.
- **Tag redundancy:** highest risk. Avoid unbounded tag-pair self-joins on dense
  `atom_tags`; generate candidates from tags with meaningful atom counts and cap
  per-tag comparisons.
- **Empty tags:** cheap enough to run live, but still bounded and indexed.
- **Missing tag overlap:** use stored semantic edges, not raw embedding all-pairs;
  cap candidate rows before Rust-side evidence expansion.
- **Near duplicates:** already based on high-similarity semantic edges; keep the
  edge query bounded and avoid loading all atom tag metadata if only 250 pairs
  are being reviewed.
- **Source duplicates:** avoid scanning and normalizing every source URL on each
  dashboard load. Prefer a normalized-source column, index, or materialized
  duplicate-source summary.
- **Broken internal links:** keep status-filtered and result-bounded.
- **Underconnected atoms:** cap semantic-edge aggregation work or materialize
  per-atom edge summaries; this can otherwise scan the semantic edge table on
  every dashboard load.

Exit criteria: every provider documents the dominant table scans/joins it uses,
has a bounded candidate strategy, and applies limits as early as practical in
SQLite and Postgres.

#### 6E.3 - Cache Or Materialized Snapshot

Introduce a per-database signal cache once the dashboard aggregate API exists.
The cache can start simple:

- cache key: database id, provider id, provider config hash, relevant data version
- cache value: normalized `KnowledgeSignal` rows plus evaluation timestamp
- short TTL for dashboard reads
- explicit invalidation on atom, tag, wiki, link, semantic-edge, provider-config,
  feedback, and signal-action changes

If invalidation becomes hard to reason about, prefer a materialized snapshot
table over in-memory cache. The existing `knowledge_signal_feedback` and action
log tables should remain the source of truth for user decisions; cached signal
rows are derived data and can be rebuilt.

Exit criteria: repeated dashboard opens within the cache window do not re-run
expensive provider queries, and dismissed/completed signals are still filtered
correctly.

#### 6E.4 - Large-Database Validation

Add a repeatable performance harness before rollout.

Test targets:

- synthetic SQLite database with 10k captured atoms
- synthetic SQLite database with 50k captured atoms if fixture generation is practical
- representative tag density, source URLs, atom links, wiki articles, and
  semantic edges capped the same way production edge generation caps them
- Postgres check for query parity and obvious missing indexes

Measure:

- dashboard aggregate endpoint latency, cold and warm
- per-provider evaluation time
- number of SQL statements per dashboard load
- peak rows considered before final widget limits
- frontend request count on dashboard open

Initial rollout budget:

- dashboard signal request count: 1
- warm dashboard signal load: comfortably sub-second on 10k atoms
- no provider should perform an unbounded all-pairs atom or tag comparison
- hidden or disabled providers should do no evaluation work

Exit criteria: the feature has measured performance on large synthetic data,
clear provider-level timing visibility, and no known unbounded dashboard-load
query path.

### Milestone 7 - Profiles And Tuning

- Add profile presets as module config bundles.
- Add "useful / not useful" lightweight feedback on signals.
- Tune default weights and thresholds from real use.
- Consider optional LLM-assisted providers only after deterministic modules have clear product value.

Exit criteria: different users can make the same signal system feel relevant to their workflows without code changes.

### Milestone 8 - Custom Signal Providers

- Add declarative custom rule types and per-database storage.
- Add template-based rule creation UI.
- Add preview before enabling a rule.
- Emit custom-rule matches as normal `KnowledgeSignal`s.
- Allow custom providers to be dashboard-only or briefing-eligible.

Exit criteria: users can encode local knowledge-quality expectations without writing code, SQL, or scripts, and custom rules participate in the same signal, dismissal, and drilldown system as built-in providers.

## Known Gaps

1. **Semantic cohesion needs a better metric.** Atomic already stores tag centroids, but centroid alone gives the center of a tag, not how tightly its atoms cluster. Use centroid-spread or member-distance metrics in a follow-up; avoid treating edge density as full semantic cohesion.
2. **Source diversity is only as good as stored metadata.** URL ingestion improvements will make wiki-candidate scoring much better.
3. **Activity-based signals may not exist yet.** Do not invent invasive tracking just for this page. Use local metadata only if Atomic already records it or if the user explicitly opts in.
4. **Scattered/broad tags are subjective.** Start with high thresholds and make those providers easy to disable.
5. **Clusters may be expensive.** Prefer existing semantic edges and simple graph statistics before adding new clustering jobs.
6. **Actions may outpace UI support.** A signal can suggest "merge tags" before a polished merge flow exists, but the initial action should degrade to "review tags" rather than creating a dead button.
7. **Briefings can become noisy.** Keep briefing suggestions capped and conservative. Dashboard widgets can hold lower-urgency queues.
8. **Custom rules can become a programming language.** Keep them declarative, template-driven, and bounded. Resist arbitrary SQL/scripts.
9. **Undo is only as good as captured state.** Mutating actions must define their undo semantics before they ship.
10. **Health framing keeps reappearing.** Link quality, boilerplate, and source duplicates are useful signals, but avoid collapsing them into an overall score or system-status badge.

## Open Questions

1. What should the dashboard widgets call this: Knowledge Quality, Opportunities, Worth Attention, or something else?
2. Should provider preferences be per-database only, or should users be able to set global defaults for new databases?
3. Should profiles appear during onboarding, in settings, or as dashboard customization?
4. How much local activity history should Atomic track for ranking, if any?
5. Should future wiki opportunity scoring use semantic edges only, raw embedding comparisons, or both?
6. Should dismissed signals be hidden forever, or reappear when their reason components materially change?
7. Which mutating actions are safe enough for direct execution with undo, and which should always be proposals?
8. Which custom rule templates should ship first?
9. Should custom rules be allowed in briefing-surface suggestions by default, or dashboard-only until the user opts in?

## Resolved Decisions

- Briefing-surface suggestions are live dashboard prompts backed by signal feedback, not persisted report-finding content.
- The existing dashboard is the surface for these signals; do not add a second health or quality dashboard.
- Wiki update suggestions should come from `wiki_update` signals, not from local dashboard atom-count deltas.
- Custom rules are valuable but should come after built-in providers, feedback state, drilldowns, and action audit are stable.
