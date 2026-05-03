//! Integration tests proving `HealthThresholds` actually change what checks flag.
//!
//! These tests create real atoms, persist per-DB `HealthConfig` with custom
//! thresholds, and assert that `compute_single_check` returns different counts
//! depending on the threshold values. They use the pure-SQL checks (no
//! embeddings / LLM required) to stay hermetic.

use atomic_core::health::{HealthConfig, HealthThresholds};
use atomic_core::{AtomicCore, CreateAtomRequest};
use tempfile::TempDir;

async fn setup() -> (AtomicCore, TempDir) {
    let dir = TempDir::new().expect("create tempdir");
    let core = AtomicCore::open_or_create(dir.path().join("test.db")).expect("open sqlite");
    (core, dir)
}

async fn create(core: &AtomicCore, content: &str) {
    core.create_atom(
        CreateAtomRequest {
            content: content.to_string(),
            ..Default::default()
        },
        |_| {},
    )
    .await
    .expect("create_atom");
}

fn count_flagged(result: &atomic_core::health::HealthCheckResult) -> usize {
    // `issues` is an object keyed by issue type (`very_short`, `very_long`,
    // `no_headings`, `no_source`, …). We only care about the length-based
    // ones since those are what HealthThresholds controls.
    let issues = match result.data.get("issues").and_then(|v| v.as_object()) {
        Some(o) => o,
        None => return 0,
    };
    let length_based = ["very_short", "very_long"];
    length_based
        .iter()
        .filter_map(|k| issues.get(*k))
        .filter_map(|issue| issue.get("count").and_then(|c| c.as_u64()))
        .map(|c| c as usize)
        .sum()
}
#[tokio::test]
async fn test_content_quality_threshold_controls_flagged_count() {
    let (core, _dir) = setup().await;

    // Three atoms: 50 chars, 150 chars, 50_000 chars.
    create(&core, &"a".repeat(50)).await;
    create(&core, &"b".repeat(150)).await;
    create(&core, &"c".repeat(50_000)).await;

    // --- Strict window: short < 200, long > 40_000. Should flag all three
    //     (50 + 150 are both short, 50_000 is long).
    let strict = HealthConfig {
        thresholds: HealthThresholds {
            content_quality_short_chars: 200,
            content_quality_long_chars: 40_000,
            ..HealthThresholds::default()
        },
        ..HealthConfig::default()
    };
    core.set_health_config(&strict)
        .await
        .expect("set strict config");

    let (_, strict_result) = atomic_core::health::compute_single_check(&core, "content_quality")
        .await
        .expect("compute content_quality (strict)");
    let strict_count = count_flagged(&strict_result);
    assert!(
        strict_count >= 3,
        "strict thresholds should flag at least the 3 seeded atoms, got {strict_count} (data={})",
        strict_result.data
    );

    // --- Lax window: short < 20, long > 100_000. Should flag none of our three.
    let lax = HealthConfig {
        thresholds: HealthThresholds {
            content_quality_short_chars: 20,
            content_quality_long_chars: 100_000,
            ..HealthThresholds::default()
        },
        ..HealthConfig::default()
    };
    core.set_health_config(&lax).await.expect("set lax config");

    let (_, lax_result) = atomic_core::health::compute_single_check(&core, "content_quality")
        .await
        .expect("compute content_quality (lax)");
    let lax_count = count_flagged(&lax_result);
    assert!(
        lax_count < strict_count,
        "lax thresholds should flag strictly fewer atoms than strict (strict={strict_count}, lax={lax_count})"
    );
}

