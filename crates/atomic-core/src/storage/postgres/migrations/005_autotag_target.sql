-- Mark which top-level tags the auto-tagger may extend with sub-tags.
ALTER TABLE tags ADD COLUMN IF NOT EXISTS is_autotag_target BOOLEAN NOT NULL DEFAULT FALSE;

-- Backfill: the five seeded categories are auto-tag targets by default.
UPDATE tags SET is_autotag_target = TRUE
  WHERE parent_id IS NULL
    AND name IN ('Topics', 'People', 'Locations', 'Organizations', 'Events');

INSERT INTO schema_version (version) VALUES (5);
