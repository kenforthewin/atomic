# Tag Properties — structured data, harvested by the pipeline

## Status

Plan, drafted 2026-08-08. Direction settled in conversation after analyzing
Logseq 2.0's DB release; no implementation yet. Supersedes the "templates &
structured atoms" framing from the Discord feature analysis — this doc is the
deliberate answer to that demand, not a transcription of it.

## Thesis

Logseq 2.0 rebuilt itself around manual structured data: a tag carries typed
properties, and tagging a node hands the *human* a form to fill. Atomic already
has the two things Logseq spent years building — a canonical SQLite store and a
tag system — plus the one thing Logseq doesn't have: a pipeline that already
reads every atom with an LLM. So we invert the fill direction:

**A tag declares the properties its atoms tend to have. The pipeline fills
them.** Tag a transcript `#meeting` (usually automatically) and `date`,
`attendees`, `decisions` get extracted in the same breath — typed, queryable,
and provenance-tracked. The human never sees a form.

The compounding loop this closes, entirely on existing features: auto-tagging
assigns the tag → extraction fills the properties → the tag's wiki prompt
(shipped v1.44) writes the running summary → chat answers property questions
deterministically instead of hoping semantic search lands on the right chunk.

## Design principles (settled — revisit deliberately, not casually)

1. **Structure is harvested, not imposed.** Declarations are lightweight hints
   that steer extraction. Nothing validates, nothing blocks a save, no atom is
   ever "malformed".
2. **The tag is the schema carrier.** No separate "type" entity. This is the
   third entry in the per-tag config seam (`autotag_description`, wiki
   prompts) and reuses its storage, routes, and context-menu patterns.
   Declarations inherit down the existing tag tree — a child tag carries its
   ancestors' declarations. No multi-parent `Extends`; the tree we have is the
   inheritance we support.
3. **Manual beats extracted, always.** User-authored frontmatter and
   panel-entered values are manual intent; extraction never overwrites them.
   Same rule re-tag established for tags.
4. **The pipeline never edits the user's text.** Extracted values live in the
   index table only. User-authored frontmatter stays in the content,
   untouched. Export materializes *both* as frontmatter so the markdown ZIP
   remains complete — the mistake to avoid is Logseq's lossy markdown export.
5. **The feature costs nothing until used.** An atom whose tags declare no
   properties adds zero LLM calls and zero UI. Cost scales with adoption, not
   with corpus size.
6. **Small type system.** `text`, `number`, `date`, `boolean`, and multi-value
   variants of text (stored as JSON arrays). No cardinality apparatus, no
   constrained-choice UI, no per-tag visibility matrix. Atom-references are
   explicitly deferred (see Open questions).

## Data model

Two tables per data database (SQLite V26, Postgres 027 — follow the V25/026
conventions exactly, including the transactional-DDL lesson from the
adversarial review of PR #228):

```
tag_property_defs
  id          TEXT PRIMARY KEY
  tag_id      TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE
  name        TEXT NOT NULL              -- snake_case key, unique per tag
  value_type  TEXT NOT NULL              -- text | number | date | boolean | text_list
  description TEXT NOT NULL DEFAULT ''   -- doubles as the extraction hint
  sort_order  INTEGER NOT NULL DEFAULT 0
  UNIQUE(tag_id, name)

atom_properties
  atom_id     TEXT NOT NULL REFERENCES atoms(id) ON DELETE CASCADE
  name        TEXT NOT NULL
  value       TEXT NOT NULL              -- canonical string; JSON array for text_list
  value_type  TEXT NOT NULL
  source      TEXT NOT NULL              -- manual | extracted
  tag_id      TEXT                       -- def-bearing tag that produced it (extracted only)
  updated_at  TEXT NOT NULL
  PRIMARY KEY (atom_id, name)
```

One row per (atom, name): when a manual value exists, extraction skips that
key entirely (principle 3 enforced at the write, not at read time). `tag_id`
on extracted rows keeps the audit trail and lets a tag's backfill re-extract
only its own keys. Postgres adds `db_id` to both tables with the same fencing
the tags queries carry — and the same two-db fencing test shape
(`pg_tag_wiki_prompts_fenced_by_db_id` is the model).

`Tag` model stays lean (the tag tree ships hundreds of rows); defs are fetched
on demand like wiki prompts. `AtomWithTags` responses gain a `properties`
array — values are small, and the reader panel needs them without a second
round trip.

## Extraction flow

Anchor: the tag-extraction step the pipeline already runs
(`embedding.rs` — `extract_tags_from_content` via
`call_structured::<ExtractionResult>` in `extraction.rs`, which already feeds
`autotag_description` into the prompt via `get_tag_tree_for_llm`).

After tag assignment resolves (auto or manual), collect the atom's assigned
tags' declarations — walking up the tag tree for inherited defs — and if any
exist, make **one** additional structured call:

- The JSON schema is *generated* from the merged declarations (name → type,
  description → field docs). Two tags declaring the same name merge to one
  field; first-writer wins on conflicting types, and a debug log records the
  collision.
- Model: the tagging model (`provider_config.llm_model()`), same tier and
  temperature philosophy as tagging — this is utility extraction, not
  agentic work.
- Writes: upsert extracted rows, skipping any key with a manual row. Emit a
  `PropertiesExtracted` event through the existing callback → broadcast
  bridge so the reader panel refreshes live.
- Failure: log-and-leave-empty, exactly like tag extraction. Never blocks the
  pipeline.

Manual tag add/remove outside the pipeline (tag chip UI) triggers the same
pass for the affected atom. Content edits re-extract on the normal pipeline
run; extracted rows are cheap to overwrite (manual rows are not touched).

## Frontmatter

On atom save, parse a leading `---` block for **flat** `key: value` pairs into
manual rows (typed by simple inference: number, ISO date, true/false, `[a, b]`
lists; everything else text). No YAML dependency and no nested structures —
the import path's frontmatter handling (`lib.rs` Obsidian import) shows the
precedent and the restraint. The chunker excludes the frontmatter block from
embeddings (`chunking.rs`). Editing a property in the reader panel writes a
manual row — it does not rewrite the user's text (principle 4). Export
(markdown ZIP, per-atom) materializes the merged property set as frontmatter.

## Backfill

Per-tag action mirroring `retag_all_atoms` / `claim_all_for_retagging_sync`:
"Extract properties" on a tag sweeps its atoms (including descendants'
atoms? — no: the tag's own atom set, descendants opt in from their own tag,
keeping cost visible), re-running extraction for that tag's keys only.
Surfaced next to "Re-tag" in whatever UI hosts it, and in the tag Properties
modal. Progress via the existing event pattern.

## Phases

**Phase 1 — declarations, extraction, display (shippable alone).**
Migrations; storage trait + both impls; defs CRUD on the per-tag config seam
(`GET/PUT /api/tags/{id}/properties`, modeled byte-for-byte on wiki-prompts);
pipeline extraction pass; frontmatter parse + chunker exclusion; reader
properties panel (read + manual edit); tag context-menu "Properties…" modal
(name / type / description rows, mirroring the wiki-prompt modal's load-gate
lessons); per-tag backfill. Export materialization.

**Phase 2 — the query surface (where the value compounds).**
Property predicates in search (`routes` + search UI filter row); an agent tool
in `chat_tools.rs` (`query_atoms_by_properties`: tag + simple predicates —
equals / contains / before / after / is-set), scope-gated like its siblings.
This is what turns chat from "semantically similar chunks" into "what's
overdue" answered from the index. List-view sort by property rides along if
cheap.

**Phase 3 — templates as capture rituals (separate doc when reached).**
A template body attached to a tag: creating an atom *from* the tag pre-fills
the shape (`{{date}}`, `{{title}}` placeholders only). Validated by Logseq's
"apply template to tags" — but deferred until phases 1–2 prove the extraction
loop, because templates without the query surface are just snippets.

## Cost model

Zero additional calls for atoms with no def-bearing tags. One structured call
per pipeline run otherwise — the same cost class as the tagging call it rides
beside. A 500-atom backfill on a tag ≈ 500 tagging-tier calls, batched and
progress-reported like re-tag. No embedding cost anywhere (properties are not
embedded; they are the *deterministic* complement to embeddings).

## Testing

The PR #228 review taught the pattern this plan inherits:

- Precedence pinned end-to-end: a wiremock test asserting the extraction
  request's generated schema contains the declared fields, and that a manual
  row survives a re-extraction that returns a different value. Each test must
  fail under the specific mutation it guards.
- Both backends for every storage behavior; PG fencing test for `db_id`; the
  PG suites now actually run in CI (test.yml — verified in #228), so new
  PG-gated tests execute.
- Migration crash-safety: transactional DDL, literal version stamps.
- Frontmatter parser: property-based tests over the flat grammar; malformed
  frontmatter degrades to "it's just content", never an error.

## Open questions (deferred, not blocking Phase 1)

- **Atom-reference type**: `attendees` pointing at person atoms would light up
  the graph, but resolution (name → atom) is fuzzy and belongs with the trust
  layer work. Ship `text` / `text_list` first; references are additive later.
- **Card/list display**: should grid cards surface a property chip (e.g. due
  date)? Decide after the reader panel exists and real usage shows which
  properties earn ambient display.
- **Wiki integration**: feeding a properties digest into wiki generation
  (deterministic tables in articles). Natural, but rides on Phase 2's query
  layer.
- **Confidence scores**: the trust-layer theme wants tagger-confidence judged
  by a second model; extracted properties would join that scheme when it
  exists. Schema reserves nothing for it — `source` can grow values later.

## Out of scope (by principle, not by laziness)

Validation and required fields; cardinality rules; constrained choices;
multi-parent inheritance; property-triggered automation (repeating tasks
etc. — the automations-vision doc owns triggers); an expression language;
nested frontmatter; writing extracted values into content.
