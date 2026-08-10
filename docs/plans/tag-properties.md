# Tag Properties — structured data, harvested by the pipeline

## Status

Plan, drafted 2026-08-08; manual-entry, lifecycle, and extraction-honesty
semantics settled in a same-day design talk-through. Direction settled in
conversation after analyzing Logseq 2.0's DB release; no implementation yet. Supersedes the "templates &
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
7. **Extraction is honest or it is silent.** Every generated schema field is
   nullable and the prompt instructs the model to fill only what the source
   states with high confidence — omit the rest. An absent value is an
   affordance (the panel invites the human to fill it); a guessed value is a
   trust bug. And some fields are categorically not the model's to fill —
   judgment fields like `priority` or `status`, where the failure mode isn't
   "the text doesn't state it" but "the text *implies* it": each declaration
   carries `auto_extract` (default on), and opted-out fields are excluded
   from the generated schema entirely — never shown to the model, not
   instructed-to-skip. Same invitation-only philosophy as auto-tag targets,
   one level down.

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
  auto_extract BOOLEAN NOT NULL DEFAULT TRUE  -- principle 7: off = manual-only field
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
only its own keys.

Keying on `(atom_id, name)` rather than `(atom_id, tag_id, name)` is a
semantic choice, not a shortcut: properties describe the **atom**; tags
contribute vocabulary. An atom tagged `#meeting` + `#project-alpha` gets the
union of both declaration sets, and two tags declaring the same name (with
the same type) converge on one field — shared names are shared vocabulary,
the way a column name means the same thing across joined tables, and either
tag's queries see the value. Same name with *different* types is a
user-fixable smell: first declaration wins, a debug log records the
collision, no UI ceremony in v1. Postgres adds `db_id` to both tables with the same fencing
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
  description → field docs) — but only from fields with `auto_extract` on;
  manual-only fields never enter the schema. Every field is nullable, and
  the prompt instructs: fill only what the source states with high
  confidence, omit everything else (principle 7). Two tags declaring the
  same name merge to one field; first-writer wins on conflicting types, and
  a debug log records the collision.
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

## Manual entry: the panel first, frontmatter for power users

The reader's properties panel is the **primary** manual path, and it is more
than an editor for existing values — declared-but-empty properties render as
affordances. An atom tagged `#meeting` whose transcript contained no date
shows `date: —` with a date picker; every declared key (including manual-only
ones, and those inherited through the tag tree) appears as a typed empty slot.
That empty slot does double duty: manual-entry invitation *and* an honest
signal about what the source actually contains (principle 7 guarantees
extraction left it absent rather than guessing). Nothing is required, nothing
blocks — the form is scaffolding, not a gate.

Panel rules:

- **Editing an extracted value converts it to manual.** The user's touch is
  manual intent; the row's provenance flips and re-extraction never claws it
  back — the per-value analog of manual tags surviving re-tag. Provenance is
  shown subtly (extracted values marked the way auto-applied tags are
  distinguished from manual ones).
- **Ad-hoc keys are allowed** ("+ add property"): undeclared properties are
  pure manual data — extraction ignores them, queries and export see them.
  Frontmatter can contain any key, so the panel must too; the two manual
  paths stay equal in expressive power.

Frontmatter remains the power-user and portability path. On atom save, a
leading `---` block is parsed for **flat** `key: value` pairs into manual rows
(typed by simple inference: number, ISO date, true/false, `[a, b]` lists;
everything else text). No YAML dependency and no nested structures — the
import path's frontmatter handling (`lib.rs` Obsidian import) shows the
precedent and the restraint. The chunker excludes the frontmatter block from
embeddings (`chunking.rs`). Panel edits write manual rows — they never
rewrite the user's text (principle 4). Export (markdown ZIP, per-atom)
materializes the merged property set as frontmatter.

## Lifecycle: untagging and declaration edits

Extracted values are a **projection** of (content × current declarations);
manual values are assertions. Every lifecycle rule follows from that split:

- **Untagging re-projects.** Removing `#meeting` drops the extracted values
  its schema produced (`tag_id` audit column) unless a remaining tag still
  declares the key. Manual values — including edit-to-own conversions —
  survive unconditionally: removing a tag doesn't make the date less true.
  The dominant untag case with an auto-tagger is mis-tag correction, and the
  extracted values are products of the same mistake; keeping them would
  leave machine-authored ghost data. Nothing is lost that matters:
  re-tagging re-extracts in one pipeline pass.
- **Deleting a declaration** from a tag sweeps that key's extracted values
  across the tag's atoms the same way; manual values survive as ad-hoc
  properties.

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
properties panel (declared-but-empty slots with typed inputs, edit-to-own,
ad-hoc keys); tag context-menu "Properties…" modal (name / type /
description / auto-extract toggle rows, mirroring the wiki-prompt modal's
load-gate lessons); untag/declaration-edit re-projection; per-tag backfill.
Export materialization.

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
