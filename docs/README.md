# Atomic Docs

This directory is organized by document intent. When adding a doc, choose the folder based on how the document should be read today, not how important the topic is.

## Manual

User-facing documentation for end-users of Atomic — installation, concepts, guides, self-hosting instructions. These are synced into the marketing website ([atomic-website](https://github.com/kenforthewin/atomic-website)) at build time and rendered as the public docs site. Edits here are the source of truth; do not edit the website's copy directly.

- [Manual contents](manual/)

## Reference

Current behavior and architecture for systems that exist in the product. These docs should be kept in sync with code changes.

- [Embedding and Auto-Tagging Pipeline](reference/embedding-tagging-pipeline.md)

## Plans

Design notes, rollout plans, and proposed product or architecture changes. These may describe future behavior or partially implemented work, so verify against the code before treating them as reference.

- [Auto-Tag Targets](plans/auto-tag-targets.md)
- [Foreign Key Enforcement](plans/foreign-keys.md)
- [Obsidian Tag Round-Trip](plans/obsidian-tag-roundtrip.md)
- [Async Migration Plan](plans/plan-async-migration.md)
- [URL Ingestion Improvements](plans/url-ingestion-improvements.md)
- [Wiki Proposal Loop Plan](plans/wiki-proposal-loop-plan.md)

## Research

Exploratory analysis and investigation notes. These are useful context, but they are not implementation specs by default.

- [LLM Wiki Gist Analysis](research/llm-wiki-gist-analysis.md)
- [Memory Retention Audit](research/memory-retention-audit.md)

## Images

Static assets used by README files and documentation live in [images](images/).
