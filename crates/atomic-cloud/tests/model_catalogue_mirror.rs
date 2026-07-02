//! Drift guard: the frontend managed-model mirror
//! (`frontend/src/lib/models.ts`) must name exactly the model ids the server
//! curates (`crate::curated_models`).
//!
//! The server is authoritative — an out-of-list choice is rejected with
//! `model_not_curated` — so drift is a UX bug, not a correctness one, but a
//! silent one: a **stale** frontend entry shows a dead picker option (its save
//! 400s), a **missing** one hides a model the account is allowed to use. This
//! test fails the build on either, so the two lists can't quietly diverge (the
//! `TODO(DASH-2)` in models.ts). It is deliberately a string scan of the mirror
//! rather than a TS parse: it collects every `'provider/model'` literal the
//! file names and compares that set to the server constants.

use std::collections::BTreeSet;

use atomic_cloud::{
    FREE_AGENTIC_MODELS, MANAGED_EMBEDDING_MODEL, MANAGED_TAGGING_MODEL, PRO_AGENTIC_MODELS,
};

/// Every `'provider/model'` id the mirror names, found by anchoring on the `/`
/// of each candidate and expanding over id-safe characters, then requiring
/// single quotes on both ends. Anchoring on `/` (rather than pairing quotes)
/// keeps apostrophes in comments ("the account's tier") and slashed paths in
/// comments (`frontend/src/lib/models.ts`, unquoted) from being mistaken for
/// ids or desyncing the scan.
fn frontend_model_ids(ts: &str) -> BTreeSet<String> {
    let bytes = ts.as_bytes();
    let is_id_char = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_');
    let mut ids = BTreeSet::new();
    for (i, &c) in bytes.iter().enumerate() {
        if c != b'/' {
            continue;
        }
        let mut left = i;
        while left > 0 && is_id_char(bytes[left - 1]) {
            left -= 1;
        }
        let mut right = i + 1;
        while right < bytes.len() && is_id_char(bytes[right]) {
            right += 1;
        }
        // Non-empty on both sides, and wrapped in single quotes.
        if left < i
            && right > i + 1
            && left > 0
            && right < bytes.len()
            && bytes[left - 1] == b'\''
            && bytes[right] == b'\''
        {
            ids.insert(ts[left..right].to_string());
        }
    }
    ids
}

#[test]
fn frontend_mirror_matches_server_catalogue() {
    // Compile-time embed: CI always has the frontend source, and this recompiles
    // whenever either the mirror or this crate's constants change.
    let ts = include_str!("../frontend/src/lib/models.ts");

    let mut server: BTreeSet<String> = BTreeSet::new();
    server.insert(MANAGED_EMBEDDING_MODEL.to_string());
    server.insert(MANAGED_TAGGING_MODEL.to_string());
    server.extend(FREE_AGENTIC_MODELS.iter().map(|s| s.to_string()));
    server.extend(PRO_AGENTIC_MODELS.iter().map(|s| s.to_string()));

    let frontend = frontend_model_ids(ts);

    let missing: Vec<&String> = server.difference(&frontend).collect();
    let stale: Vec<&String> = frontend.difference(&server).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "frontend model mirror drifted from crate::curated_models — keep \
         frontend/src/lib/models.ts in lockstep.\n  \
         missing from frontend (model hidden from the picker): {missing:?}\n  \
         stale in frontend (dead picker option, its save 400s): {stale:?}"
    );
}
