//! Regression tests for broken-internal-links scope.
//!
//! Covers the gap where the link resolver only tried current-dir and
//! vault-root candidates for bare markdown hrefs. Atoms living in a sibling
//! subdirectory were incorrectly flagged as broken even though the picker
//! (which does a vault-wide title/URL search) could find them.
//!
//! After the fix, a bare markdown href that misses both exact-candidate
//! lookups falls back to a vault-wide filename-stem search mirroring the
//! wikilink resolution path.

mod support;

use atomic_core::health::compute::compute_single_check;
use atomic_core::CreateAtomRequest;
use support::{setup_core, Backend, MockAiServer};

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

fn count_from_check(data: &serde_json::Value) -> i64 {
    data.get("broken_links")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| {
            data.get("broken_link_list")
                .and_then(|v| v.as_array())
                .map(|a| a.len() as i64)
                .unwrap_or(0)
        })
}

/// A bare markdown link `[x](glossary.md)` in an atom inside `references/`
/// must resolve against a target atom living in `shared/glossary.md` — not
/// be reported as broken.
#[tokio::test]
async fn broken_links_resolves_sibling_subdirectory_markdown() {
    let mock = MockAiServer::start().await;
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    // Target lives in a different subdirectory than the source. The old
    // resolver would only try `references/glossary.md` and
    // `vault://notes/glossary.md`, missing this one.
    let _target = make_atom(
        core,
        "# Glossary\n\nTerms.",
        Some("vault://notes/shared/glossary.md"),
    )
    .await;

    let _source = make_atom(
        core,
        "see [glossary](./glossary.md) for terms",
        Some("vault://notes/references/onboarding.md"),
    )
    .await;

    let (_, result) = compute_single_check(core, "broken_internal_links")
        .await
        .expect("compute");

    assert_eq!(
        count_from_check(&result.data),
        0,
        "expected 0 broken links after subdir fallback; data={:?}",
        result.data,
    );
}

/// Control: a markdown link whose stem does not exist anywhere in the vault
/// must still be reported as broken.
#[tokio::test]
async fn broken_links_still_flags_truly_missing_markdown() {
    let mock = MockAiServer::start().await;
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    let _source = make_atom(
        core,
        "see [missing](./totally-missing-xyz.md) here",
        Some("vault://notes/references/note.md"),
    )
    .await;

    let (_, result) = compute_single_check(core, "broken_internal_links")
        .await
        .expect("compute");

    assert!(
        count_from_check(&result.data) >= 1,
        "truly-missing markdown href must still be flagged; data={:?}",
        result.data,
    );
}
