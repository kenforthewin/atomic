//! Integration tests for custom health checks wired into `compute_health`.
//!
//! These exercise the full path that unit tests in `health::custom` can't
//! reach: persistence via the settings table, key prefixing, weight
//! propagation into `aggregate_score`, informational semantics, and
//! collision avoidance between built-in and custom check keys.

use atomic_core::health::custom::{result_key, CustomCheck, CustomRule, DomainMatchMode};
use atomic_core::{AtomicCore, CreateAtomRequest};
use tempfile::TempDir;

async fn setup() -> (AtomicCore, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let core = AtomicCore::open_or_create(dir.path().join("test.db")).expect("open sqlite");
    (core, dir)
}

async fn make_atom(core: &AtomicCore, content: &str, source: Option<&str>) -> String {
    let atom = core
        .create_atom(
            CreateAtomRequest {
                content: content.to_string(),
                source_url: source.map(|s| s.to_string()),
                published_at: None,
                tag_ids: vec![],
                skip_if_source_exists: false,
            },
            |_| {},
        )
        .await
        .expect("create_atom")
        .expect("created");
    atom.atom.id
}

fn check(id: &str, weight: f64, rule: CustomRule) -> CustomCheck {
    CustomCheck {
        id: id.to_string(),
        label: id.to_string(),
        description: String::new(),
        enabled: true,
        weight,
        rule,
    }
}

// --- Persistence ------------------------------------------------------------

#[tokio::test]
async fn custom_checks_round_trip_via_settings() {
    let (core, _dir) = setup().await;

    assert!(core.get_custom_health_checks().await.unwrap().is_empty());

    let checks = vec![
        check("c1", 0.0, CustomRule::RequireSource { tag_filter: None }),
        check(
            "c2",
            0.5,
            CustomRule::ContentLength {
                min_words: 10,
                max_words: 0,
                tag_filter: None,
            },
        ),
    ];
    core.set_custom_health_checks(&checks).await.unwrap();

    let loaded = core.get_custom_health_checks().await.unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].id, "c1");
    assert_eq!(loaded[1].id, "c2");
    assert!((loaded[1].weight - 0.5).abs() < f64::EPSILON);
}

// --- compute_health wires custom checks into the report --------------------

#[tokio::test]
async fn compute_health_includes_custom_check_with_prefixed_key() {
    let (core, _dir) = setup().await;
    make_atom(&core, "no source", None).await;
    make_atom(&core, "has source", Some("https://example.com/x")).await;

    core.set_custom_health_checks(&[check(
        "needs_source",
        0.0,
        CustomRule::RequireSource { tag_filter: None },
    )])
    .await
    .unwrap();

    let report = core.compute_health().await.expect("compute_health");
    let key = result_key("needs_source");
    assert_eq!(key, "custom.needs_source");

    let res = report.checks.get(&key).expect("custom check present");
    assert_eq!(res.data["total_considered"], 2);
    assert_eq!(res.data["flagged_count"], 1);
    assert_eq!(res.status, "error");
    assert!(res.requires_review);
}

// --- Zero-weight / disabled ------------------------------------------------

#[tokio::test]
async fn zero_weight_custom_check_is_informational_and_not_scored() {
    let (core, _dir) = setup().await;
    make_atom(&core, "no source", None).await;
    make_atom(&core, "has source", Some("https://x.com/a")).await;

    // Zero weight → informational. Should not drag overall_score below
    // what it would have been without the rule.
    core.set_custom_health_checks(&[check(
        "info_only",
        0.0,
        CustomRule::RequireSource { tag_filter: None },
    )])
    .await
    .unwrap();
    let with_info = core.compute_health().await.unwrap();
    let res = with_info.checks.get("custom.info_only").unwrap();
    assert!(res.informational, "zero-weight rule must be informational");

    // Wipe rule → recompute baseline score.
    core.set_custom_health_checks(&[]).await.unwrap();
    let baseline = core.compute_health().await.unwrap();

    assert_eq!(
        with_info.overall_score, baseline.overall_score,
        "informational custom check must not affect overall score"
    );
}

#[tokio::test]
async fn positive_weight_custom_check_lowers_overall_score() {
    let (core, _dir) = setup().await;
    // 3 atoms, one sourced → 2/3 flagged → check score ~33.
    make_atom(&core, "a", None).await;
    make_atom(&core, "b", None).await;
    make_atom(&core, "c", Some("https://x.com/c")).await;

    core.set_custom_health_checks(&[]).await.unwrap();
    let baseline = core.compute_health().await.unwrap();

    core.set_custom_health_checks(&[check(
        "scored",
        1.0,
        CustomRule::RequireSource { tag_filter: None },
    )])
    .await
    .unwrap();
    let with_rule = core.compute_health().await.unwrap();

    let res = with_rule.checks.get("custom.scored").unwrap();
    assert!(!res.informational, "positive weight must be scored");
    assert!(res.score < 100);

    assert!(
        with_rule.overall_score < baseline.overall_score,
        "positive-weight failing rule must drop overall score \
         (baseline={}, with_rule={})",
        baseline.overall_score,
        with_rule.overall_score,
    );
}

#[tokio::test]
async fn disabled_custom_check_does_not_appear_in_report() {
    let (core, _dir) = setup().await;
    make_atom(&core, "no source", None).await;

    let mut chk = check("c1", 1.0, CustomRule::RequireSource { tag_filter: None });
    chk.enabled = false;
    core.set_custom_health_checks(&[chk]).await.unwrap();

    let report = core.compute_health().await.expect("compute_health");
    assert!(
        !report.checks.contains_key("custom.c1"),
        "disabled rule must not be evaluated"
    );
}

// --- Key collision avoidance ------------------------------------------------

#[tokio::test]
async fn custom_check_cannot_collide_with_builtin_key() {
    let (core, _dir) = setup().await;
    make_atom(&core, "x", None).await;

    // Custom rule with an id that matches a built-in check name — prefix
    // must prevent collision.
    core.set_custom_health_checks(&[check(
        "tag_health",
        1.0,
        CustomRule::RequireSource { tag_filter: None },
    )])
    .await
    .unwrap();

    let report = core.compute_health().await.unwrap();
    // Built-in check survives untouched.
    assert!(report.checks.contains_key("tag_health"));
    // Custom check lands under prefixed key.
    assert!(report.checks.contains_key("custom.tag_health"));

    let builtin = &report.checks["tag_health"];
    let custom = &report.checks["custom.tag_health"];
    // Different shapes — builtin has no "custom" flag.
    assert!(custom.data.get("custom").and_then(|v| v.as_bool()).unwrap_or(false));
    assert!(builtin.data.get("custom").is_none());
}

// --- Multi-rule batch -------------------------------------------------------

#[tokio::test]
async fn multiple_custom_checks_all_evaluate_independently() {
    let (core, _dir) = setup().await;
    make_atom(&core, "short", None).await;
    make_atom(
        &core,
        "one two three four five six seven eight nine ten eleven twelve",
        Some("https://arxiv.org/abs/1"),
    )
    .await;
    make_atom(
        &core,
        "plenty of words here for the length check",
        Some("https://reddit.com/r/x"),
    )
    .await;

    core.set_custom_health_checks(&[
        check("src", 0.0, CustomRule::RequireSource { tag_filter: None }),
        check(
            "len",
            0.0,
            CustomRule::ContentLength {
                min_words: 5,
                max_words: 0,
                tag_filter: None,
            },
        ),
        check(
            "dom",
            0.0,
            CustomRule::SourceDomainMatches {
                domains: vec!["arxiv.org".into()],
                mode: DomainMatchMode::Allowlist,
                tag_filter: None,
            },
        ),
    ])
    .await
    .unwrap();

    let report = core.compute_health().await.unwrap();
    let src = &report.checks["custom.src"];
    let len = &report.checks["custom.len"];
    let dom = &report.checks["custom.dom"];

    // One atom without source.
    assert_eq!(src.data["flagged_count"], 1);
    // "short" has 1 word (<5).
    assert_eq!(len.data["flagged_count"], 1);
    // Allowlist = arxiv.org → reddit.com is flagged; no-source atom is
    // skipped by SourceDomainMatches (has no source).
    assert_eq!(dom.data["flagged_count"], 1);
}

// --- Fault isolation --------------------------------------------------------

#[tokio::test]
async fn malformed_regex_does_not_break_builtin_checks() {
    let (core, _dir) = setup().await;
    make_atom(&core, "x", None).await;

    core.set_custom_health_checks(&[check(
        "bad_regex",
        1.0,
        CustomRule::ContentRegex {
            pattern: "(?P<unterminated".into(), // guaranteed-invalid regex
            invert: false,
        },
    )])
    .await
    .unwrap();

    // Report must still compute; bad rule is logged and dropped.
    let report = core.compute_health().await.expect("compute_health survives");
    assert!(!report.checks.is_empty());
    // The malformed rule should not appear as a successful result.
    let custom_keys: Vec<_> = report
        .checks
        .keys()
        .filter(|k| k.starts_with("custom."))
        .collect();
    assert!(
        custom_keys.is_empty() || !custom_keys.iter().any(|k| k.as_str() == "custom.bad_regex"),
        "malformed rule must not leak a bogus successful result; got {custom_keys:?}"
    );
}
