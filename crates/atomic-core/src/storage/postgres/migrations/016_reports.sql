-- Reports primitive — phase 2 of docs/plans/reports.md.
--
-- Three tables introduced together because their lifecycles are linked: a
-- finding atom always points back at a `report_findings` provenance row,
-- which always points back at a `reports` definition (until the report is
-- deleted, at which point provenance survives via ON DELETE SET NULL).
--
-- All three are per-DB. Column types mirror the SQLite schema as plain TEXT
-- timestamps and TEXT JSON, matching the convention the rest of this
-- Postgres schema uses.

CREATE TABLE IF NOT EXISTS reports (
    id                    TEXT PRIMARY KEY,
    name                  TEXT NOT NULL,
    description           TEXT,
    research_prompt       TEXT NOT NULL,
    source_scope_tag_ids  TEXT NOT NULL DEFAULT '[]',
    source_scope_window   TEXT,
    source_include_kinds  TEXT NOT NULL DEFAULT '["captured"]',
    context_scope_mode    TEXT NOT NULL DEFAULT 'all',
    context_scope_tag_ids TEXT NOT NULL DEFAULT '[]',
    context_scope_window  TEXT,
    context_include_kinds TEXT NOT NULL DEFAULT '["captured"]',
    citation_policy       TEXT NOT NULL DEFAULT 'source_only',
    max_source_atoms      INTEGER,
    max_source_tokens     INTEGER,
    max_tool_iterations   INTEGER,
    schedule              TEXT NOT NULL,
    schedule_tz           TEXT,
    enabled               INTEGER NOT NULL DEFAULT 1,
    output_atom_tags      TEXT NOT NULL DEFAULT '[]',
    last_run_at           TEXT,
    last_finding_atom_id  TEXT,
    last_error            TEXT,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    db_id                 TEXT NOT NULL DEFAULT 'default'
);

CREATE INDEX IF NOT EXISTS idx_reports_enabled
    ON reports(db_id, enabled, last_run_at);

CREATE TABLE IF NOT EXISTS report_findings (
    finding_atom_id      TEXT PRIMARY KEY,
    report_id            TEXT,
    run_id               TEXT,
    report_name_snapshot TEXT NOT NULL,
    created_at           TEXT NOT NULL,
    db_id                TEXT NOT NULL DEFAULT 'default',
    FOREIGN KEY (finding_atom_id) REFERENCES atoms(id) ON DELETE CASCADE,
    FOREIGN KEY (report_id) REFERENCES reports(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_report_findings_report_created
    ON report_findings(db_id, report_id, created_at DESC);

CREATE TABLE IF NOT EXISTS report_finding_citations (
    finding_atom_id TEXT NOT NULL,
    cited_atom_id   TEXT NOT NULL,
    position        INTEGER NOT NULL,
    excerpt         TEXT NOT NULL,
    db_id           TEXT NOT NULL DEFAULT 'default',
    PRIMARY KEY (finding_atom_id, cited_atom_id, position),
    FOREIGN KEY (finding_atom_id) REFERENCES atoms(id) ON DELETE CASCADE,
    FOREIGN KEY (cited_atom_id)   REFERENCES atoms(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_finding_citations_cited
    ON report_finding_citations(db_id, cited_atom_id);

INSERT INTO schema_version (version) VALUES (16);
