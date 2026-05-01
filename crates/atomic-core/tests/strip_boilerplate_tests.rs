mod support;

use atomic_core::CreateAtomRequest;
use support::{setup_core, Backend, MockAiServer};

#[tokio::test]
async fn strip_boilerplate_dry_run_does_not_mutate() {
    let mock = MockAiServer::start().await;
    mock.mock_chat_completion("Only the unique part.").await;
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    let result = core
        .create_atom(
            CreateAtomRequest {
                content: "# Template\n## Subject\nOnly the unique part.\n".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create")
        .expect("atom inserted");

    let (proposed, action) =
        atomic_core::health::llm_fixes::strip_boilerplate_atom(core, &result.atom.id, true)
            .await
            .expect("strip dry");

    assert!(proposed.contains("unique part"), "proposed: {proposed}");
    assert!(action.is_none(), "dry_run must not emit a FixAction");

    let reloaded = core
        .get_atom(&result.atom.id)
        .await
        .expect("get")
        .expect("exists");
    assert!(
        reloaded.atom.content.contains("# Template"),
        "content unchanged in dry_run"
    );
}

#[tokio::test]
async fn strip_boilerplate_apply_mutates_and_logs_fix() {
    let mock = MockAiServer::start().await;
    mock.mock_chat_completion("Stripped content.").await;
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    let result = core
        .create_atom(
            CreateAtomRequest {
                content: "Original with template cruft.".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create")
        .expect("atom inserted");

    let (proposed, action) =
        atomic_core::health::llm_fixes::strip_boilerplate_atom(core, &result.atom.id, false)
            .await
            .expect("strip apply");

    assert_eq!(proposed, "Stripped content.");
    assert!(action.is_some(), "apply must return a FixAction");

    let reloaded = core
        .get_atom(&result.atom.id)
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(reloaded.atom.content, "Stripped content.");
}

#[tokio::test]
async fn strip_boilerplate_rejects_empty_response() {
    let mock = MockAiServer::start().await;
    mock.mock_chat_completion("EMPTY").await;
    let handle = setup_core(Backend::Sqlite, &mock.base_url())
        .await
        .expect("harness");
    let core = &handle.core;

    let result = core
        .create_atom(
            CreateAtomRequest {
                content: "All boilerplate.".to_string(),
                ..Default::default()
            },
            |_| {},
        )
        .await
        .expect("create")
        .expect("atom inserted");

    let err =
        atomic_core::health::llm_fixes::strip_boilerplate_atom(core, &result.atom.id, false)
            .await
            .expect_err("must reject EMPTY");

    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("boilerplate") || msg.to_lowercase().contains("empty"),
        "unexpected error message: {msg}"
    );
}
