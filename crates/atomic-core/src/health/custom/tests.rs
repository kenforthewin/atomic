//! Custom-rule evaluator tests.

use super::*;
use crate::db::Database;
use crate::storage::sqlite::SqliteStorage;
use rusqlite::params;
use std::sync::Arc;

fn make_storage() -> (SqliteStorage, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    let db = Arc::new(Database::open(&path).unwrap());
    (SqliteStorage::new(db), tmp)
}

fn insert_atom(conn: &rusqlite::Connection, id: &str, content: &str, source: Option<&str>) {
    conn.execute(
        "INSERT INTO atoms (id, content, source_url, embedding_status, tagging_status, created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'complete', 'complete', datetime('now'), datetime('now'))",
        params![id, content, source],
    )
    .unwrap();
}

fn insert_tag(conn: &rusqlite::Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO tags (id, name, parent_id, created_at, is_autotag_target) VALUES (?1, ?2, NULL, datetime('now'), 0)",
        params![id, name],
    )
    .unwrap();
}

fn link(conn: &rusqlite::Connection, atom: &str, tag: &str) {
    conn.execute(
        "INSERT INTO atom_tags (atom_id, tag_id) VALUES (?1, ?2)",
        params![atom, tag],
    )
    .unwrap();
}

#[test]
fn require_source_flags_atoms_without_url() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "has source", Some("https://x.com/a"));
        insert_atom(&conn, "a2", "no source", None);
        insert_atom(&conn, "a3", "blank source", Some(""));
    }

    let check = CustomCheck {
        id: "c1".into(),
        label: "needs source".into(),
        description: String::new(),
        enabled: true,
        weight: 0.0,
        rule: CustomRule::RequireSource { tag_filter: None },
    };
    let out = run_all(&storage, &[check.clone()]).unwrap();
    assert_eq!(out.len(), 1);
    let (_, result, _) = &out[0];
    let data = &result.data;
    assert_eq!(data["total_considered"], 3);
    assert_eq!(data["flagged_count"], 2);
    assert_eq!(result.status, "error");
}

#[test]
fn tag_requires_flags_atoms_missing_required_tag() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "paper with source", Some("https://x"));
        insert_atom(&conn, "a2", "paper no source", None);
        insert_tag(&conn, "t_paper", "paper");
        insert_tag(&conn, "t_sourced", "sourced");
        link(&conn, "a1", "t_paper");
        link(&conn, "a1", "t_sourced");
        link(&conn, "a2", "t_paper");
    }

    let check = CustomCheck {
        id: "c1".into(),
        label: "papers need sourced".into(),
        description: String::new(),
        enabled: true,
        weight: 0.0,
        rule: CustomRule::TagRequires {
            any_of: vec!["t_paper".into()],
            required: vec!["t_sourced".into()],
        },
    };
    let out = run_all(&storage, &[check.clone()]).unwrap();
    assert_eq!(out.len(), 1);
    let (_, result, _) = &out[0];
    assert_eq!(result.data["total_considered"], 2);
    assert_eq!(result.data["flagged_count"], 1);
    // Only a2 is flagged.
    let flagged = result.data["flagged"].as_array().unwrap();
    assert_eq!(flagged.len(), 1);
    assert_eq!(flagged[0]["id"], "a2");
}

#[test]
fn content_regex_with_invert_flags_atoms_not_matching() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "has TODO inside", None);
        insert_atom(&conn, "a2", "no markers here", None);
    }

    let check = CustomCheck {
        id: "c1".into(),
        label: "no TODO in notes".into(),
        description: String::new(),
        enabled: true,
        weight: 0.0,
        rule: CustomRule::ContentRegex {
            pattern: r"TODO".into(),
            invert: false,
        },
    };
    let out = run_all(&storage, &[check.clone()]).unwrap();
    let (_, result, _) = &out[0];
    assert_eq!(result.data["flagged_count"], 1);
    assert_eq!(result.data["flagged"][0]["id"], "a1");

    let inverted = CustomCheck {
        rule: CustomRule::ContentRegex {
            pattern: r"TODO".into(),
            invert: true,
        },
        ..check.clone()
    };
    let out = run_all(&storage, &[inverted]).unwrap();
    let (_, result, _) = &out[0];
    assert_eq!(result.data["flagged_count"], 1);
    assert_eq!(result.data["flagged"][0]["id"], "a2");
}

#[test]
fn disabled_checks_are_skipped() {
    let (storage, _tmp) = make_storage();
    let check = CustomCheck {
        id: "c1".into(),
        label: "anything".into(),
        description: String::new(),
        enabled: false,
        weight: 0.0,
        rule: CustomRule::RequireSource { tag_filter: None },
    };
    let out = run_all(&storage, &[check.clone()]).unwrap();
    assert!(out.is_empty());
}

#[test]
fn zero_weight_produces_informational_result() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "x", None);
    }
    let check = CustomCheck {
        id: "c1".into(),
        label: "l".into(),
        description: String::new(),
        enabled: true,
        weight: 0.0,
        rule: CustomRule::RequireSource { tag_filter: None },
    };
    let out = run_all(&storage, &[check.clone()]).unwrap();
    assert!(out[0].1.informational);

    let scored = CustomCheck {
        weight: 0.2,
        ..check
    };
    let out = run_all(&storage, &[scored]).unwrap();
    assert!(!out[0].1.informational);
}

// ---- Tier 1 ----

fn check_with(rule: CustomRule) -> CustomCheck {
    CustomCheck {
        id: "c1".into(),
        label: "l".into(),
        description: String::new(),
        enabled: true,
        weight: 0.0,
        rule,
    }
}

#[test]
fn require_tag_flags_untagged_atoms() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "tagged", None);
        insert_atom(&conn, "a2", "bare", None);
        insert_tag(&conn, "t_topic", "topic");
        link(&conn, "a1", "t_topic");
    }
    let check = check_with(CustomRule::RequireTag {
        any_of: vec!["t_topic".into()],
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["flagged_count"], 1);
    assert_eq!(r.data["flagged"][0]["id"], "a2");
}

#[test]
fn content_length_flags_too_short_and_too_long() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "one two three four five six", None);  // 6 words, OK
        insert_atom(&conn, "a2", "tiny", None);                         // 1 word
        insert_atom(&conn, "a3", &"w ".repeat(50), None);               // 50 words
    }
    let check = check_with(CustomRule::ContentLength {
        min_words: 5,
        max_words: 30,
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["flagged_count"], 2);
    let flagged: Vec<&str> = r.data["flagged"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert!(flagged.contains(&"a2"));
    assert!(flagged.contains(&"a3"));
}

#[test]
fn citation_count_flags_atoms_with_too_few_links() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "one [link](http://a) and [[wiki]]", None);
        insert_atom(&conn, "a2", "no citations here at all", None);
        insert_atom(&conn, "a3", "only [one](http://x) here", None);
    }
    let check = check_with(CustomRule::CitationCount {
        min_citations: 2,
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["flagged_count"], 2);
    let flagged: Vec<&str> = r.data["flagged"]
        .as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert!(flagged.contains(&"a2"));
    assert!(flagged.contains(&"a3"));
}

#[test]
fn source_domain_allowlist_flags_off_list_domains() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "paper", Some("https://arxiv.org/abs/1"));
        insert_atom(&conn, "a2", "blog", Some("https://random.example/post"));
        insert_atom(&conn, "a3", "no source skipped", None);
    }
    let check = check_with(CustomRule::SourceDomainMatches {
        domains: vec!["arxiv.org".into()],
        mode: DomainMatchMode::Allowlist,
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    // a3 is skipped (no source); a1 on allowlist; a2 off.
    assert_eq!(r.data["total_considered"], 2);
    assert_eq!(r.data["flagged_count"], 1);
    assert_eq!(r.data["flagged"][0]["id"], "a2");
}

#[test]
fn source_domain_blocklist_flags_on_list_domains() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "reddit", Some("https://old.reddit.com/r/x"));
        insert_atom(&conn, "a2", "arxiv", Some("https://arxiv.org/abs/1"));
    }
    let check = check_with(CustomRule::SourceDomainMatches {
        domains: vec!["reddit.com".into()],
        mode: DomainMatchMode::Blocklist,
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["flagged_count"], 1);
    assert_eq!(r.data["flagged"][0]["id"], "a1");
}

#[test]
fn stale_atom_flags_old_tagged_atoms() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_tag(&conn, "t_draft", "draft");
        // Old: 30 days ago
        let old = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        conn.execute(
            "INSERT INTO atoms (id, content, source_url, embedding_status, tagging_status, created_at, updated_at) \
             VALUES ('a1', 'stale', NULL, 'complete', 'complete', ?1, ?1)",
            params![old],
        ).unwrap();
        // Fresh: now
        insert_atom(&conn, "a2", "fresh", None);
        link(&conn, "a1", "t_draft");
        link(&conn, "a2", "t_draft");
    }
    let check = check_with(CustomRule::StaleAtom {
        tag: "t_draft".into(),
        max_age_days: 14,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["total_considered"], 2);
    assert_eq!(r.data["flagged_count"], 1);
    assert_eq!(r.data["flagged"][0]["id"], "a1");
}

// ---- Tier 2 ----

#[test]
fn forbidden_combo_flags_atoms_carrying_all_forbidden_tags() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a1", "both", None);
        insert_atom(&conn, "a2", "only draft", None);
        insert_tag(&conn, "t_draft", "draft");
        insert_tag(&conn, "t_published", "published");
        link(&conn, "a1", "t_draft");
        link(&conn, "a1", "t_published");
        link(&conn, "a2", "t_draft");
    }
    let check = check_with(CustomRule::ForbiddenTagCombo {
        all_of: vec!["t_draft".into(), "t_published".into()],
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["flagged_count"], 1);
    assert_eq!(r.data["flagged"][0]["id"], "a1");
}

#[test]
fn missing_heading_flags_long_atoms_without_heading() {
    let (storage, _tmp) = make_storage();
    let long = "x".repeat(200);
    let with_h = format!("# Title\n{}", "y".repeat(200));
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "short", "too short to flag", None);
        insert_atom(&conn, "no_h", &long, None);
        insert_atom(&conn, "has_h", &with_h, None);
    }
    let check = check_with(CustomRule::MissingHeading {
        min_length_chars: 120,
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    assert_eq!(r.data["flagged_count"], 1);
    assert_eq!(r.data["flagged"][0]["id"], "no_h");
}

#[test]
fn tag_cardinality_flags_over_and_under_tagged() {
    let (storage, _tmp) = make_storage();
    {
        let conn = storage.db.conn.lock().unwrap();
        insert_atom(&conn, "a0", "no tags", None);
        insert_atom(&conn, "a1", "one tag", None);
        insert_atom(&conn, "a2", "two tags", None);
        insert_atom(&conn, "a5", "five tags", None);
        for i in 0..5 {
            insert_tag(&conn, &format!("t{i}"), &format!("t{i}"));
        }
        link(&conn, "a1", "t0");
        link(&conn, "a2", "t0");
        link(&conn, "a2", "t1");
        for i in 0..5 {
            link(&conn, "a5", &format!("t{i}"));
        }
    }
    let check = check_with(CustomRule::TagCardinality {
        min: 1,
        max: 3,
        tag_filter: None,
    });
    let out = run_all(&storage, &[check]).unwrap();
    let (_, r, _) = &out[0];
    let flagged: Vec<&str> = r.data["flagged"]
        .as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert!(flagged.contains(&"a0"));  // under min
    assert!(flagged.contains(&"a5"));  // over max
    assert!(!flagged.contains(&"a1"));
    assert!(!flagged.contains(&"a2"));
}
