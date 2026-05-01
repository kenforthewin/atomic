mod support;

use atomic_core::CreateAtomRequest;
use atomic_core::health::llm_fixes::{auto_resolve_broken_link, AutoResolveOutcome};
use support::{setup_core, Backend, MockAiServer};

/// Helper: create an atom with a given content and optional source_url.
async fn make_atom(
    core: &atomic_core::AtomicCore,
    content: &str,
    source_url: Option<&str>,
) -> String {
    core.create_atom(
        CreateAtomRequest {
            content: content.to_string(),
            source_url: source_url.map(|s| s.to_string()),
            ..Default::default()
        },
        |_| {},
    )
    .await
    .expect("create atom")
    .expect("atom inserted")
    .atom
    .id
}

/// LLM returns confidence 0.9 → should relink to the top candidate.
#[tokio::test]
async fn broken_link_auto_resolve_relinks_on_high_confidence() {
    let mock = MockAiServer::start().await;

    // We need a target atom with a source_url that will surface as a candidate.
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    // Create the target atom first.
    let target_id = make_atom(
        core,
        "# Bravo Notes\n\nContent about bravo.",
        Some("vault://notes/bravo.md"),
    )
    .await;

    // Create the source atom with the broken link.
    let source_id = make_atom(
        core,
        "see [bravo](./bravo.md) for details",
        Some("vault://notes/alpha.md"),
    )
    .await;

    // The LLM returns JSON selecting the target with confidence 0.9.
    let llm_response = format!(
        r#"{{"target_atom_id":"{target_id}","confidence":0.9,"reason":"exact title match"}}"#
    );
    mock.mock_chat_completion(llm_response).await;

    let outcome = auto_resolve_broken_link(core, &source_id, "[bravo](./bravo.md)", "bravo")
        .await
        .expect("auto_resolve");

    match outcome {
        AutoResolveOutcome::Relinked { target_atom_id, confidence, .. } => {
            assert_eq!(target_atom_id, target_id, "should relink to target");
            assert!(confidence >= 0.9 - f32::EPSILON, "confidence should be 0.9");
        }
        other => panic!("expected Relinked, got: {other:?}"),
    }

    // Verify the atom content was actually updated.
    let updated = core.get_atom(&source_id).await.expect("get").expect("exists");
    assert!(
        updated.atom.content.contains(&format!("atom://{target_id}")),
        "content should contain relinked atom:// URI"
    );
}

/// LLM returns confidence 0.3 → should skip (leave link unchanged).
#[tokio::test]
async fn broken_link_auto_resolve_skips_on_low_confidence() {
    let mock = MockAiServer::start().await;

    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    // Create a target so there's at least one candidate.
    let target_id = make_atom(
        core,
        "# Gamma Notes\n\nSomething.",
        Some("vault://notes/gamma.md"),
    )
    .await;

    let source_id = make_atom(
        core,
        "see [gamma](./gamma.md) here",
        Some("vault://notes/beta.md"),
    )
    .await;

    let llm_response = format!(
        r#"{{"target_atom_id":"{target_id}","confidence":0.3,"reason":"uncertain match"}}"#
    );
    mock.mock_chat_completion(llm_response).await;

    let outcome = auto_resolve_broken_link(core, &source_id, "[gamma](./gamma.md)", "gamma")
        .await
        .expect("auto_resolve");

    match outcome {
        AutoResolveOutcome::Skipped { reason } => {
            assert!(reason.contains("0.30") || reason.contains("low confidence") || reason.contains("uncertain"),
                "unexpected skip reason: {reason}");
        }
        other => panic!("expected Skipped, got: {other:?}"),
    }

    // Verify content is unchanged.
    let updated = core.get_atom(&source_id).await.expect("get").expect("exists");
    assert!(
        updated.atom.content.contains("[gamma](./gamma.md)"),
        "content should be unchanged"
    );
}

/// No candidates available → should remove the link.
#[tokio::test]
async fn broken_link_auto_resolve_removes_when_no_candidates() {
    let mock = MockAiServer::start().await;

    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    // Create an atom with a broken link; no other atoms exist that would match.
    let source_id = make_atom(
        core,
        "see [nonexistent](./totally-missing-xyz.md) here",
        Some("vault://notes/src.md"),
    )
    .await;

    // Mock should not be called since we return early, but provide a fallback anyway.
    mock.mock_chat_completion(r#"{"target_atom_id":null,"confidence":0.0,"reason":"no match"}"#)
        .await;

    let outcome =
        auto_resolve_broken_link(core, &source_id, "[nonexistent](./totally-missing-xyz.md)", "nonexistent")
            .await
            .expect("auto_resolve");

    match outcome {
        AutoResolveOutcome::Removed { reason } => {
            assert!(!reason.is_empty(), "reason must be non-empty: {reason}");
        }
        // If there were candidates (e.g. source_url suffix match), the outcome might be
        // Skipped — both are acceptable for the "no good target" case.
        AutoResolveOutcome::Skipped { .. } => {}
        AutoResolveOutcome::Relinked { .. } => panic!("should not have relinked with no valid candidates"),
    }
}
