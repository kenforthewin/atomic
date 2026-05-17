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

## Computation Strategy

Start with manual/on-demand evaluation:

1. User opens the dashboard.
2. Server evaluates enabled providers for the active database.
3. Results are normalized, filtered by feedback state, ranked, and returned to dashboard widgets.

Briefing integration should use the same signal infrastructure:

1. Daily briefing generation determines its normal content window.
2. Atomic evaluates briefing-eligible providers using that window where relevant.
3. The top few signals are attached to the briefing response as structured suggestions.
4. The UI renders those suggestions beside or below the generated briefing content.

Do not make the LLM responsible for deciding which knowledge-quality suggestions to show. It can summarize recent atoms; deterministic providers should pick the actions.

Add caching once evaluation becomes expensive. A simple cache invalidation model is enough:

- atom created/updated/deleted
- atom tags changed
- tag renamed/merged/deleted
- wiki generated/updated
- semantic edges rebuilt

For v1, it is acceptable to recompute all enabled deterministic dashboard providers for the active database when the dashboard opens, then cache for a short period. Briefing-attached signals should be stored with the briefing or reproducible from the same evaluation window so historical briefings do not drift.

## API Surface

Core:

- `AtomicCore::list_knowledge_signals(filter)`
- `AtomicCore::get_knowledge_signal(signal_key)`
- `AtomicCore::list_briefing_knowledge_signals(window, limit)`
- `AtomicCore::set_knowledge_signal_provider_config(provider_id, config)`
- `AtomicCore::dismiss_knowledge_signal(signal_key)`
- `AtomicCore::snooze_knowledge_signal(signal_key, until)`
- `AtomicCore::restore_knowledge_signal(signal_key)`
- Later: `AtomicCore::preview_custom_signal_rule(rule)`
- Later: `AtomicCore::apply_knowledge_signal_action(signal_key, action)`
- Later: `AtomicCore::undo_knowledge_signal_action(action_log_id)`

Server:

- `GET /api/knowledge-signals`
- `GET /api/knowledge-signals/:signal_key`
- `GET /api/knowledge-signals/briefing-candidates`
- `PATCH /api/knowledge-signals/providers/:provider_id`
- `POST /api/knowledge-signals/:signal_key/dismiss`
- `POST /api/knowledge-signals/:signal_key/snooze`
- `POST /api/knowledge-signals/:signal_key/restore`
- Later: `POST /api/knowledge-signals/custom/preview`
- Later: `POST /api/knowledge-signals/:signal_key/actions`
- Later: `POST /api/knowledge-signals/actions/:action_log_id/undo`

Frontend transport should expose these as normal commands. The React page should stay transport-agnostic.

Briefing read shapes should eventually include attached signal suggestions. That can be done either by extending `BriefingWithCitations` with a `signals` field or by adding a companion route keyed by briefing ID. Extending the read shape is cleaner if the suggestions are persisted with the briefing.

## Milestones

### Milestone 1 - Signal Foundation

- Add shared signal models in `atomic-core`.
- Add provider registry with deterministic providers.
- Add provider config and feedback storage.
- Add server routes and transport commands.
- Implement dismiss, snooze, restore, and provider enable/disable.
- Build the first dashboard widget backed by signals.
- Upgrade `NewWikisWidget` to use `WikiCandidateProvider` ranking rather than atom count.

Exit criteria: the existing dashboard can show deterministic signals from at least one provider, explain them, and remember when the user dismisses or snoozes them. There is no new dashboard route.

### Milestone 2 - Briefing Suggestions

- Add `include_in_briefing` provider config.
- Evaluate briefing-eligible signals during briefing generation or briefing fetch.
- Attach the top signals to the briefing as structured suggestions.
- Render a "Worth your attention" section in `BriefingWidget` / `BriefingContent`.
- Keep deterministic suggestions separate from the LLM-generated briefing prose.

Exit criteria: a generated briefing can include 3-5 actionable knowledge-quality suggestions, each with deterministic reasons and direct actions. Historical briefings show the same attached suggestions when revisited.

### Milestone 3 - Better Wiki Opportunities

- Replace or augment the current wiki suggestions with `WikiCandidateProvider`.
- Score wiki candidates using atom volume, source diversity, cohesion, breadth, recency, and wiki absence.
- Add `WikiUpdateProvider` for existing articles worth updating.
- Wire suggested actions to existing wiki generation/update flows.

Exit criteria: wiki suggestions are no longer ranked only by literal atom count, and each suggestion explains why it is worth generating or updating.

### Milestone 4 - Organization Signals

- Add conservative tag-redundancy detection using atom overlap, semantic overlap, hierarchy context, and name similarity.
- Add empty-tag and low-value tag cleanup signals.
- Add initial scattered-tag detection based on semantic cohesion.
- Add UI actions for opening the tag, reviewing atoms, and starting merge/cleanup flows where those flows exist.

Exit criteria: users can identify and dismiss high-confidence tag redundancy and cleanup opportunities without the system making subjective cleanup decisions automatically.

### Milestone 5 - Connection And Strength Signals

- Add isolated atom signals that require evidence of nearby clusters.
- Add "missing tag overlap" signals for atoms close to a tag cluster.
- Add thin-but-central concept signals.
- Consider optional use of search/chat history if local metadata exists and the user has enabled the module.

Exit criteria: Atomic can surface underconnected or underdeveloped knowledge areas without confusing them with processing failures.

### Milestone 6 - Link And Source Quality

- Add broken internal link detection as a dashboard-first provider.
- Add source URL duplicate and content-overlap providers.
- Add boilerplate/source-pollution detection where the signal can be deterministic.
- Add reviewed proposal flow for any content rewrite or boilerplate stripping action.
- Add audit/undo infrastructure before any direct mutating action ships.

Exit criteria: Atomic can surface link and source-quality issues with concrete evidence and safe review actions, without treating them as system health failures or auto-fixing content by default.

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
5. Should wiki opportunity scoring use semantic edges only, raw embedding comparisons, or both?
6. Should dismissed signals be hidden forever, or reappear when their reason components materially change?
7. Should briefing-attached suggestions be persisted with each briefing, or recomputed from the briefing window when the briefing is viewed?
8. Which mutating actions are safe enough for direct execution with undo, and which should always be proposals?
9. Which custom rule templates should ship first?
10. Should custom rules be allowed in briefing suggestions by default, or dashboard-only until the user opts in?
