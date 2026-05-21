-- V22 marker. The briefings → finding-atom data migration and the subsequent
-- DROP of `briefings` / `briefing_citations` are owned by the Rust path
-- (`crate::reports::seed::migrate_briefings_to_findings`), which runs at
-- server startup with a per-DB idempotency flag. A pure SQL drop here would
-- discard history before the Rust path could rehome it.
SELECT 1;
