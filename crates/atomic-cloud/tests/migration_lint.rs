//! The additive-only migration lint (plan: "Schema migration on deploy" →
//! "Rolling deploys"; Decisions log 2026-05-25: "Migrations are
//! additive-only ... Enforced by a CI lint on migration SQL").
//!
//! Rolling deploys and the structurally-safe rollback story both rest on
//! one invariant: a binary at version M must keep working against a schema
//! at M+1. Additive changes (ADD COLUMN, CREATE TABLE, CREATE INDEX,
//! deferred/not-validated constraints) preserve that — old code ignores
//! columns and tables it doesn't know about. The forms this lint forbids
//! are exactly the ones that change or remove the relational surface a
//! deployed binary addresses by name:
//!
//! - `DROP TABLE` / `DROP COLUMN` — old queries break outright.
//! - `ALTER COLUMN ... TYPE` (and `SET DATA TYPE`) — old code decodes the
//!   wire type it was compiled against (sqlx is strict; see core migration
//!   020's own header for the failure mode this caused in reverse).
//! - `RENAME` (tables, columns, indexes, constraints) — a rename is a drop
//!   and an add wearing a trenchcoat.
//!
//! Deliberately **allowed**: `DROP INDEX`, `DROP CONSTRAINT`, and
//! `DROP TRIGGER` — relaxations and replace-patterns that change no name
//! any binary's SQL addresses (queries never reference indexes; dropping a
//! constraint only permits more; `DROP TRIGGER` + `CREATE TRIGGER` is the
//! idempotent re-create idiom core migration 001 itself uses).
//!
//! The plan's escape hatch — "drops happen N+1 deploys later, after all
//! referring code is out of the fleet" — is [`EXEMPT`]: a deliberate,
//! per-file waiver asserting exactly that. The lint keeps the list honest
//! both ways: an exempt file must exist *and still violate* (a stale entry
//! fails the lint), so the list can only carry real, current waivers.
//!
//! Scope: both migration directories this binary applies — atomic-cloud's
//! control-plane migrations and atomic-core's tenant migrations (read via
//! path; the fleet runner applies the latter to every tenant database).
//! This test is deliberately NOT Postgres-gated: it reads SQL off disk and
//! must run on every `cargo test`, cluster or no cluster.

use std::fmt;
use std::path::{Path, PathBuf};

/// Per-file waivers for deliberate non-additive migrations. Adding an entry
/// is asserting the plan's N+1 discipline: **no binary that can reach a
/// database this migration applies to still references the changed
/// objects.** Each entry must name a file that exists and that the lint
/// would otherwise flag.
const EXEMPT: &[(&str, &str)] = &[(
    "atomic-core/020_atom_positions_double.sql",
    "REAL → DOUBLE PRECISION widening; predates the 2026-05-25 additive-only \
     decision and the cloud fleet, and is already applied everywhere — \
     rewriting applied history would only lie",
)];

/// One forbidden form found in masked migration SQL.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    /// 1-based source line of the form's first token.
    line: usize,
    /// Which forbidden form matched.
    form: &'static str,
    /// The trimmed source line, for the failure message.
    snippet: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {} — `{}`", self.line, self.form, self.snippet)
    }
}

/// Blank out everything that is not live SQL — `--` line comments, nested
/// `/* */` block comments, `'...'` string literals (with `''` escapes),
/// `"..."` quoted identifiers (with `""` escapes), and `$tag$...$tag$`
/// dollar-quoted bodies — replacing masked characters with spaces and
/// preserving newlines, so token line numbers survive masking.
fn mask_sql(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;

    /// Emit one masked character: newlines survive, all else blanks.
    fn blank(out: &mut String, c: char) {
        out.push(if c == '\n' { '\n' } else { ' ' });
    }

    /// The `$tag$` delimiter starting at `chars[i]`, if any: `$` followed
    /// by zero or more tag characters and a closing `$`.
    fn dollar_delimiter(chars: &[char], i: usize) -> Option<usize> {
        debug_assert_eq!(chars[i], '$');
        let mut j = i + 1;
        while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        (j < chars.len() && chars[j] == '$').then_some(j + 1 - i)
    }

    while i < chars.len() {
        match chars[i] {
            // Line comment: blank to end of line.
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    blank(&mut out, chars[i]);
                    i += 1;
                }
            }
            // Block comment: Postgres block comments nest.
            '/' if chars.get(i + 1) == Some(&'*') => {
                let mut depth = 0;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        blank(&mut out, chars[i]);
                        blank(&mut out, chars[i + 1]);
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        blank(&mut out, chars[i]);
                        blank(&mut out, chars[i + 1]);
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        blank(&mut out, chars[i]);
                        i += 1;
                    }
                }
            }
            // String literal / quoted identifier: doubled-quote escapes.
            quote @ ('\'' | '"') => {
                blank(&mut out, chars[i]);
                i += 1;
                while i < chars.len() {
                    if chars[i] == quote {
                        if chars.get(i + 1) == Some(&quote) {
                            blank(&mut out, chars[i]);
                            blank(&mut out, chars[i + 1]);
                            i += 2;
                        } else {
                            blank(&mut out, chars[i]);
                            i += 1;
                            break;
                        }
                    } else {
                        blank(&mut out, chars[i]);
                        i += 1;
                    }
                }
            }
            // Dollar-quoted body: blank through the matching delimiter.
            '$' => {
                if let Some(delim_len) = dollar_delimiter(&chars, i) {
                    let delimiter: String = chars[i..i + delim_len].iter().collect();
                    let rest: String = chars[i + delim_len..].iter().collect();
                    let body_len = rest.find(&delimiter).map(|p| p + delimiter.len());
                    let end = match body_len {
                        Some(len) => i + delim_len + len,
                        None => chars.len(), // unterminated: mask to EOF
                    };
                    while i < end {
                        blank(&mut out, chars[i]);
                        i += 1;
                    }
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Uppercased word tokens (`[A-Za-z0-9_]+`) of masked SQL, each with its
/// 1-based source line.
fn tokenize(masked: &str) -> Vec<(String, usize)> {
    let mut tokens = Vec::new();
    let mut line = 1;
    let mut current = String::new();
    let mut current_line = 1;
    for c in masked.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            if current.is_empty() {
                current_line = line;
            }
            current.push(c.to_ascii_uppercase());
        } else {
            if !current.is_empty() {
                tokens.push((std::mem::take(&mut current), current_line));
            }
            if c == '\n' {
                line += 1;
            }
        }
    }
    if !current.is_empty() {
        tokens.push((current, current_line));
    }
    tokens
}

/// Lint one migration's SQL. Returns every forbidden form found.
fn lint_sql(sql: &str) -> Vec<Violation> {
    let source_lines: Vec<&str> = sql.lines().collect();
    let snippet = |line: usize| -> String {
        source_lines
            .get(line - 1)
            .map(|l| l.trim().to_string())
            .unwrap_or_default()
    };

    let tokens = tokenize(&mask_sql(sql));
    let tok = |i: usize| tokens.get(i).map(|(t, _)| t.as_str());

    let mut violations = Vec::new();
    for (i, (token, line)) in tokens.iter().enumerate() {
        let form = match token.as_str() {
            "RENAME" => Some("RENAME"),
            "DROP" => match tok(i + 1) {
                Some("TABLE") => Some("DROP TABLE"),
                Some("COLUMN") => Some("DROP COLUMN"),
                _ => None,
            },
            // `ALTER COLUMN <name> TYPE ...` / `ALTER COLUMN <name> SET
            // DATA TYPE ...` — other ALTER COLUMN forms (SET DEFAULT, SET
            // NOT NULL, DROP DEFAULT) leave the wire type alone.
            "ALTER" if tok(i + 1) == Some("COLUMN") => match (tok(i + 3), tok(i + 4), tok(i + 5)) {
                (Some("TYPE"), _, _) => Some("ALTER COLUMN TYPE"),
                (Some("SET"), Some("DATA"), Some("TYPE")) => Some("ALTER COLUMN TYPE"),
                _ => None,
            },
            _ => None,
        };
        if let Some(form) = form {
            violations.push(Violation {
                line: *line,
                form,
                snippet: snippet(*line),
            });
        }
    }
    violations
}

/// Read and lint every `*.sql` file in `dir`, labeled `prefix/<file_name>`.
/// Returns `(files_scanned, violations_per_file)`, file order sorted for
/// deterministic output.
fn lint_dir(prefix: &str, dir: &Path) -> (usize, Vec<(String, Vec<Violation>)>) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading migration dir {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    paths.sort();

    let mut results = Vec::new();
    for path in &paths {
        let sql = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let label = format!(
            "{prefix}/{}",
            path.file_name().expect("file name").to_string_lossy()
        );
        let violations = lint_sql(&sql);
        if !violations.is_empty() {
            results.push((label, violations));
        }
    }
    (paths.len(), results)
}

/// THE lint: every migration this binary applies — control-plane and
/// tenant — is additive-only, modulo the explicitly waived [`EXEMPT`] files
/// (each of which must really exist and really violate).
#[test]
fn migration_sql_is_additive_only() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cloud_dir = manifest.join("migrations");
    let core_dir = manifest.join("../atomic-core/src/storage/postgres/migrations");

    let (cloud_files, cloud_violations) = lint_dir("atomic-cloud", &cloud_dir);
    let (core_files, core_violations) = lint_dir("atomic-core", &core_dir);

    // A moved directory must fail loudly, not pass vacuously.
    assert!(
        cloud_files >= 9,
        "expected at least the 9 control-plane migrations in {}, found {cloud_files}",
        cloud_dir.display()
    );
    assert!(
        core_files >= 22,
        "expected at least the 22 tenant migrations in {}, found {core_files}",
        core_dir.display()
    );

    let mut flagged: Vec<(String, Vec<Violation>)> = cloud_violations
        .into_iter()
        .chain(core_violations)
        .collect();

    // Exemption hygiene: every waiver must point at a file that exists and
    // still violates — otherwise the waiver is stale and must be removed.
    for (exempt_label, reason) in EXEMPT {
        let position = flagged.iter().position(|(label, _)| label == exempt_label);
        assert!(
            position.is_some(),
            "stale EXEMPT entry {exempt_label:?} ({reason}): the file is \
             missing or no longer violates the lint; remove the entry"
        );
        flagged.remove(position.expect("checked above"));
    }

    assert!(
        flagged.is_empty(),
        "non-additive migration SQL breaks rolling deploys and rollback \
         (plan: \"Schema migration on deploy\"). Make the change additive, \
         or — ONLY for an N+1 drop whose referring code is provably out of \
         the fleet — add an EXEMPT waiver.\n{}",
        flagged
            .iter()
            .flat_map(|(label, violations)| violations
                .iter()
                .map(move |v| format!("  {label}: {v}")))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ==================== Lint honesty: fixture probes ====================
//
// The lint is only as good as its scanner; these prove each forbidden form
// is caught and each masking rule actually masks. A regression here means
// the green main test above proves nothing.

#[track_caller]
fn assert_flags(sql: &str, form: &str) {
    let violations = lint_sql(sql);
    assert!(
        violations.iter().any(|v| v.form == form),
        "expected {form:?} to be flagged in {sql:?}; got {violations:?}"
    );
}

#[track_caller]
fn assert_clean(sql: &str) {
    let violations = lint_sql(sql);
    assert!(
        violations.is_empty(),
        "expected no violations in {sql:?}; got {violations:?}"
    );
}

#[test]
fn catches_every_forbidden_form() {
    assert_flags("DROP TABLE briefings;", "DROP TABLE");
    assert_flags("DROP TABLE IF EXISTS briefings;", "DROP TABLE");
    assert_flags("ALTER TABLE atoms DROP COLUMN kind;", "DROP COLUMN");
    assert_flags(
        "ALTER TABLE atom_positions ALTER COLUMN x TYPE DOUBLE PRECISION;",
        "ALTER COLUMN TYPE",
    );
    assert_flags(
        "ALTER TABLE atom_positions ALTER COLUMN x SET DATA TYPE DOUBLE PRECISION USING x::double precision;",
        "ALTER COLUMN TYPE",
    );
    assert_flags("ALTER TABLE atoms RENAME TO notes;", "RENAME");
    assert_flags("ALTER TABLE atoms RENAME COLUMN body TO content;", "RENAME");
    assert_flags("ALTER INDEX idx_atoms RENAME TO idx_notes;", "RENAME");
    // Case-insensitive, whitespace- and newline-tolerant.
    assert_flags("alter table t\n    drop\n    column c;", "DROP COLUMN");
    // The real 020 text is caught (the EXEMPT waiver, not scanner blindness,
    // is what admits it).
    let migration_020 = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../atomic-core/src/storage/postgres/migrations/020_atom_positions_double.sql"
    ));
    assert_flags(migration_020, "ALTER COLUMN TYPE");
}

#[test]
fn allowed_relaxations_and_additive_forms_pass() {
    assert_clean("ALTER TABLE atoms ADD COLUMN IF NOT EXISTS kind TEXT;");
    assert_clean("CREATE TABLE IF NOT EXISTS reports (id TEXT PRIMARY KEY);");
    assert_clean("CREATE INDEX IF NOT EXISTS idx ON atoms (kind);");
    assert_clean("DROP INDEX IF EXISTS idx_tags_name_parent;");
    assert_clean("ALTER TABLE settings DROP CONSTRAINT IF EXISTS settings_pkey;");
    assert_clean("DROP TRIGGER IF EXISTS atom_tags_insert_count ON atom_tags;");
    assert_clean("ALTER TABLE atoms ALTER COLUMN kind SET DEFAULT 'note';");
    assert_clean("ALTER TABLE atoms ALTER COLUMN kind DROP DEFAULT;");
    assert_clean("DELETE FROM settings WHERE db_id = '_global';");
}

#[test]
fn comments_strings_and_identifiers_do_not_false_positive() {
    // Line and block comments — including nested block comments.
    assert_clean("-- A pure SQL DROP TABLE here would discard history\nSELECT 1;");
    assert_clean("/* DROP COLUMN */ ALTER TABLE t ADD COLUMN c TEXT;");
    assert_clean("/* outer /* DROP TABLE t; */ still a comment */ SELECT 1;");
    // String literals (with the doubled-quote escape) and quoted identifiers.
    assert_clean("INSERT INTO settings (key, value) VALUES ('note', 'we may RENAME later');");
    assert_clean("INSERT INTO log (msg) VALUES ('it''s a DROP TABLE story');");
    assert_clean("CREATE TABLE t (\"rename\" TEXT);");
    // Dollar-quoted function bodies (tagged and untagged).
    assert_clean(
        "CREATE OR REPLACE FUNCTION f() RETURNS TRIGGER AS $$\n\
         BEGIN EXECUTE 'DROP TABLE scratch'; RETURN NEW; END;\n\
         $$ LANGUAGE plpgsql;",
    );
    assert_clean("DO $body$ BEGIN PERFORM 'RENAME'; END $body$;");
    // Identifiers merely containing forbidden words are whole-token misses.
    assert_clean("ALTER TABLE t ADD COLUMN drop_table_marker TEXT;");
    assert_clean("CREATE TABLE rename_log (id TEXT);");
    // `$1`-style placeholders are not dollar-quote openers; what follows
    // them is still scanned.
    assert_flags("SELECT $1; ALTER TABLE t DROP COLUMN c;", "DROP COLUMN");
    // ...and a violation AFTER a masked region is still caught.
    assert_flags(
        "-- harmless comment\nALTER TABLE t DROP COLUMN c; /* tail */",
        "DROP COLUMN",
    );
}

#[test]
fn violations_carry_useful_locations() {
    let sql = "ALTER TABLE t ADD COLUMN a TEXT;\n\
               -- comment line\n\
               ALTER TABLE t\n    DROP COLUMN b;";
    let violations = lint_sql(sql);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].form, "DROP COLUMN");
    assert_eq!(violations[0].line, 4, "line of the form's first token");
    assert_eq!(violations[0].snippet, "DROP COLUMN b;");
}
