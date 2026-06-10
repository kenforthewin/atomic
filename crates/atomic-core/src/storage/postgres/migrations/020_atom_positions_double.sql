-- Migration 020: Widen atom_positions.x / .y from REAL to DOUBLE PRECISION
--
-- `AtomPosition` is `{ x: f64, y: f64 }` everywhere in the codebase. The
-- SQLite schema has stored them as 8-byte floats since v1 (SQLite's REAL
-- is always 64-bit), and the `save_atom_positions` PG path binds f64
-- values. The original PG schema declared the columns as `REAL`, which is
-- 32-bit in Postgres — sqlx's strict type checking then fails the GET
-- path with a type-mismatch error when it tries to decode REAL back into
-- f64. Result: PUT silently rounded, GET returned 500.
--
-- The fix widens the columns to DOUBLE PRECISION so the wire format
-- matches the Rust type on both backends. Existing rows up-cast cleanly.

ALTER TABLE atom_positions
    ALTER COLUMN x TYPE DOUBLE PRECISION USING x::double precision,
    ALTER COLUMN y TYPE DOUBLE PRECISION USING y::double precision;

INSERT INTO schema_version (version) VALUES (20);
