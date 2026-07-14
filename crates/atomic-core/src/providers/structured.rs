//! Unified entry point for structured-output LLM calls.
//!
//! Every call site that wants typed JSON output (wiki synthesis, briefing
//! final pass, tag extraction, tag consolidation, ...) should go through
//! [`call_structured`] instead of threading its own retry/parse/fallback
//! loop. The function applies a portable-schema lint, drives the primary
//! structured call with retry, tolerantly parses the response (handling
//! markdown code fences and surrounding prose), and falls back to a
//! schema-in-prompt call if the first attempt produces unparseable text.
//!
//! # Portability constraints
//!
//! Schemas passed to this module must follow the "lowest common denominator"
//! rules enforced by [`lint_schema`]:
//!
//! - No `oneOf` / `anyOf` — some providers (notably OpenRouter-routed local
//!   models) don't handle union types reliably.
//! - No nullable unions (`"type": ["string", "null"]`). Use empty-string
//!   sentinels for optional fields instead.
//! - Object nodes must set `additionalProperties: false`.
//! - Every declared property must appear in `required` (OpenAI strict mode).
//!
//! These are caught at call time and surface as [`StructuredCallError::SchemaLint`].
//!
//! # What this doesn't do
//!
//! - **Streaming**. Structured output and streaming are mutually exclusive in
//!   every provider backend we ship. Callers that need streaming should use
//!   `complete_streaming_with_tools` directly and do their own parsing.
//! - **Model-capability gating**. The capabilities cache in
//!   [`super::models::ModelCapabilitiesCache`] knows which OpenRouter models
//!   advertise `structured_outputs`, but we don't gate here — the fallback
//!   path covers models that silently ignore `response_format`, and a hard
//!   gate would break local/OpenAI-compat setups where capabilities aren't
//!   reported at all.

use crate::providers::error::ProviderError;
use crate::providers::traits::{LlmConfig, LlmProvider};
use crate::providers::types::{GenerationParams, Message, StructuredOutputSchema};
use crate::providers::{get_llm_provider, ProviderConfig};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::marker::PhantomData;
use std::sync::Arc;

// ==================== Public API ====================

/// Default output-token budget for LLM calls that lack an explicit one.
/// Never leave `max_tokens` unset on a call whose output can be long:
/// OpenAI-family models treat "absent" as "model maximum", but Anthropic's
/// API *requires* the field, so routers fill in a small default — which
/// truncated a claude-authored report digest mid-sentence (2026-07-14).
///
/// This is a CEILING, not a target — generation stops at the model's
/// natural end and only generated tokens bill, so the number should be
/// generous. 32k accommodates the longest wiki article or digest with
/// room to spare while still bounding a runaway generation's cost; every
/// model on the curated list (and effectively every modern OpenRouter
/// endpoint) accepts it. Short-output callers (tagging, extraction) are
/// nowhere near it.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_000;

/// A single structured-output LLM call. Construct with [`StructuredCall::new`],
/// optionally adjust via the `with_*` methods, then pass to [`call_structured`].
pub struct StructuredCall<'a, T> {
    pub provider_config: &'a ProviderConfig,
    pub model: &'a str,
    pub messages: &'a [Message],
    pub schema_name: &'static str,
    pub schema: Value,
    pub params: GenerationParams,
    pub max_retries: usize,
    /// Whether to request OpenAI-style strict schema enforcement. Default: `true`.
    /// Strict mode guarantees the response adheres to the schema via
    /// constrained decoding, but narrows the routable provider pool on
    /// OpenRouter (since `provider.require_parameters` is also set). Opt out
    /// with [`StructuredCall::with_strict`] when calling a model that's
    /// known to reject strict mode (typically smaller OSS models routed via
    /// OpenRouter or non-OpenAI backends). The linter rules + prompt-based
    /// fallback still cover correctness when strict is off.
    pub strict: bool,
    _marker: PhantomData<fn() -> T>,
}

impl<'a, T> StructuredCall<'a, T> {
    /// Create a new call with sensible defaults (temperature 0.3, 2 retries,
    /// strict schema enforcement on).
    pub fn new(
        provider_config: &'a ProviderConfig,
        model: &'a str,
        messages: &'a [Message],
        schema_name: &'static str,
        schema: Value,
    ) -> Self {
        Self {
            provider_config,
            model,
            messages,
            schema_name,
            schema,
            params: GenerationParams::new()
                .with_temperature(0.3)
                .with_max_tokens(DEFAULT_MAX_OUTPUT_TOKENS),
            max_retries: 2,
            strict: true,
            _marker: PhantomData,
        }
    }

    /// Override the generation params (temperature, max_tokens, etc.). The
    /// `structured_output` field is always set by [`call_structured`] —
    /// anything you set here is overwritten.
    pub fn with_params(mut self, params: GenerationParams) -> Self {
        self.params = params;
        self
    }

    /// Override the retry count on transient provider errors. Default: 2.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Override the strict-mode flag. Default: `true`.
    ///
    /// Set to `false` when the target model or provider route is known to
    /// reject OpenAI strict schemas (e.g. OpenRouter routing to a smaller
    /// OSS model via vLLM). The primary call still passes the schema via
    /// `response_format`, just without the constrained-decoding guarantee —
    /// parse failures then fall through to the prompt-based fallback path.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

/// Errors returned by [`call_structured`]. Callers should match on the variant
/// so they can log / surface it sensibly — `ParseFailed` in particular carries
/// a preview of the unparseable response for debugging.
#[derive(Debug, thiserror::Error)]
pub enum StructuredCallError {
    #[error("schema lint failed: {0}")]
    SchemaLint(String),

    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("failed to parse structured output after {attempts} attempt(s): {parse_error}")]
    ParseFailed {
        parse_error: String,
        preview: String,
        attempts: usize,
    },

    #[error("{0}")]
    Other(String),
}

impl StructuredCallError {
    /// Short, single-line representation suitable for propagating up as a
    /// `String` error (most of our call sites return `Result<T, String>`).
    pub fn to_compact_string(&self) -> String {
        match self {
            StructuredCallError::ParseFailed {
                parse_error,
                preview,
                attempts,
            } => {
                format!(
                    "parse failed after {} attempt(s): {} (preview: {})",
                    attempts,
                    parse_error,
                    preview.chars().take(200).collect::<String>(),
                )
            }
            other => other.to_string(),
        }
    }
}

/// Run a structured-output call against the configured provider. See the
/// module-level docs for the full semantics.
///
/// Lifecycle:
///
/// 1. Lint the schema for portability violations. Errors return immediately.
/// 2. Build an `LlmConfig` with `structured_output` wired into the caller's
///    `GenerationParams`.
/// 3. Call `provider.complete()`. On `is_retryable()` errors, back off and
///    retry up to `max_retries` times.
/// 4. Tolerantly parse the response (raw → strip code fences → locate first
///    `{…}` substring).
/// 5. If parsing fails, fire a single prompt-based fallback call *without*
///    `structured_output`, appending an explicit "reply with ONLY valid JSON
///    matching this schema" nudge. Parse tolerantly again.
/// 6. Return the typed value or a [`StructuredCallError::ParseFailed`] with
///    the preview of the last unparseable response.
pub async fn call_structured<T: DeserializeOwned>(
    call: StructuredCall<'_, T>,
) -> Result<T, StructuredCallError> {
    let provider = get_llm_provider(call.provider_config)?;
    call_structured_with_provider(call, provider).await
}

/// Test-facing variant of [`call_structured`] that accepts an explicit
/// provider implementation. Production code should use [`call_structured`]
/// (which resolves the provider from `ProviderConfig`). This form exists so
/// unit tests can inject a mock and drive the full retry / parse / fallback
/// pipeline deterministically without a real LLM.
///
/// Keeping both entry points means production callers don't pay a plumbing
/// cost and the test surface stays fully isolated from real providers.
pub async fn call_structured_with_provider<T: DeserializeOwned>(
    call: StructuredCall<'_, T>,
    provider: Arc<dyn LlmProvider>,
) -> Result<T, StructuredCallError> {
    lint_schema(&call.schema)?;

    let StructuredCall {
        model,
        messages,
        schema_name,
        schema,
        params,
        max_retries,
        strict,
        ..
    } = call;

    // Primary attempt: with structured output enabled. Strict defaults to
    // true but callers can opt out via `StructuredCall::with_strict(false)`
    // for models that reject OpenAI strict mode.
    let schema_wrapper = StructuredOutputSchema {
        name: schema_name.to_string(),
        schema: schema.clone(),
        strict,
    };
    let structured_params = params.clone().with_structured_output(schema_wrapper);
    let primary_config = LlmConfig::new(model.to_string()).with_params(structured_params);

    let mut last_preview = String::new();
    let mut last_parse_err = String::new();

    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay = 1u64 << attempt;
            tracing::warn!(
                attempt,
                max_retries,
                delay_secs = delay,
                last_error = %last_parse_err,
                schema_name,
                "[structured] Retrying transient provider error"
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }

        match provider.complete(messages, &primary_config).await {
            Ok(response) => {
                if response.content.is_empty() {
                    return Err(StructuredCallError::Other(
                        "LLM returned empty content".to_string(),
                    ));
                }
                match parse_tolerant::<T>(&response.content) {
                    Ok(value) => return Ok(value),
                    Err(parse_err) => {
                        last_preview = preview_of(&response.content);
                        last_parse_err = parse_err.to_string();
                        tracing::warn!(
                            schema_name,
                            parse_error = %last_parse_err,
                            preview = %last_preview,
                            "[structured] Primary parse failed, will try prompt-based fallback"
                        );
                        // Parse errors don't retry on the same call shape — break out
                        // of the retry loop and try the fallback instead.
                        break;
                    }
                }
            }
            Err(e) => {
                last_parse_err = e.to_string();
                if e.is_retryable() && attempt < max_retries {
                    continue;
                }
                return Err(StructuredCallError::Provider(e));
            }
        }
    }

    // Prompt-based fallback: no schema, explicit user nudge. This handles the
    // case where the provider silently ignored `response_format` (some weaker
    // OpenRouter-routed models, some Ollama models) and returned prose or
    // fenced JSON that the primary attempt couldn't cleanly parse.
    let schema_str = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| schema.to_string());
    let nudge = format!(
        "Your previous response could not be parsed. Reply with ONLY a single JSON \
         object matching this schema. No markdown, no prose, no code fences, no \
         surrounding text.\n\nSchema:\n{}",
        schema_str
    );

    let mut fallback_messages = messages.to_vec();
    fallback_messages.push(Message::user(nudge));
    let fallback_config = LlmConfig::new(model.to_string()).with_params(params);

    match provider
        .complete(&fallback_messages, &fallback_config)
        .await
    {
        Ok(response) => match parse_tolerant::<T>(&response.content) {
            Ok(value) => {
                tracing::info!(schema_name, "[structured] Fallback parse succeeded");
                Ok(value)
            }
            Err(parse_err) => Err(StructuredCallError::ParseFailed {
                parse_error: parse_err.to_string(),
                preview: preview_of(&response.content),
                attempts: 2,
            }),
        },
        Err(e) => {
            // Fallback couldn't even reach the provider. Surface the ORIGINAL
            // parse failure as the primary diagnostic (it's the more actionable
            // signal) but mention the fallback failure in the error text.
            Err(StructuredCallError::ParseFailed {
                parse_error: format!(
                    "{} (fallback provider call also failed: {})",
                    last_parse_err, e
                ),
                preview: last_preview,
                attempts: 2,
            })
        }
    }
}

// ==================== Schema linting ====================

/// Validate that a JSON schema follows the portable subset we ship. Returns
/// an error describing every violation at once so authors can fix them in
/// a single pass rather than playing whack-a-mole.
pub fn lint_schema(schema: &Value) -> Result<(), StructuredCallError> {
    let errors = collect_lint_errors(schema, "");
    if errors.is_empty() {
        Ok(())
    } else {
        Err(StructuredCallError::SchemaLint(errors.join("; ")))
    }
}

fn collect_lint_errors(node: &Value, path: &str) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(obj) = node.as_object() else {
        if let Some(arr) = node.as_array() {
            for (i, v) in arr.iter().enumerate() {
                let child = format!("{}[{}]", path, i);
                errors.extend(collect_lint_errors(v, &child));
            }
        }
        return errors;
    };

    let here = if path.is_empty() { "<root>" } else { path };

    if obj.contains_key("oneOf") {
        errors.push(format!(
            "{}: oneOf is not portable — use a flat schema with a discriminator field",
            here
        ));
    }
    if obj.contains_key("anyOf") {
        errors.push(format!(
            "{}: anyOf is not portable — use a flat schema with a discriminator field",
            here
        ));
    }

    if let Some(type_val) = obj.get("type") {
        if let Some(type_arr) = type_val.as_array() {
            let has_null = type_arr.iter().any(|v| v.as_str() == Some("null"));
            if has_null {
                errors.push(format!(
                    "{}: nullable union types are not portable — use an empty-string sentinel instead",
                    here
                ));
            }
        }
    }

    if obj.get("type").and_then(|v| v.as_str()) == Some("object") {
        if obj.get("additionalProperties").and_then(|v| v.as_bool()) != Some(false) {
            errors.push(format!(
                "{}: object must set \"additionalProperties\": false (OpenAI strict mode)",
                here
            ));
        }
        let properties = obj.get("properties").and_then(|v| v.as_object());
        let required: Vec<&str> = obj
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if let Some(props) = properties {
            for prop in props.keys() {
                if !required.iter().any(|r| r == prop) {
                    errors.push(format!(
                        "{}: property '{}' must appear in \"required\" (use empty-string sentinels for optional fields)",
                        here, prop
                    ));
                }
            }
        }
    }

    for (key, value) in obj {
        // Skip re-linting the required/type arrays themselves — they're metadata,
        // not nested schemas.
        if key == "required" || key == "type" || key == "enum" {
            continue;
        }
        let child = if path.is_empty() {
            key.clone()
        } else {
            format!("{}.{}", path, key)
        };
        errors.extend(collect_lint_errors(value, &child));
    }

    errors
}

// ==================== Tolerant parsing ====================

/// Try hard to parse `content` as a `T`. In order:
///
/// 1. Raw `serde_json::from_str`.
/// 2. Strip surrounding markdown code fences (``` ```json ... ``` ```) and retry.
/// 3. Locate the first `{` and last `}` and parse that substring, stripping
///    any leading apology or trailing "I hope that helps!" prose.
///
/// Returns the parse error from step 3 if all attempts fail.
pub fn parse_tolerant<T: DeserializeOwned>(content: &str) -> Result<T, serde_json::Error> {
    // Attempt 1: raw parse.
    if let Ok(v) = serde_json::from_str::<T>(content) {
        return Ok(v);
    }

    // Attempt 2: strip code fences if present.
    let stripped = strip_code_fences(content);
    if stripped != content {
        if let Ok(v) = serde_json::from_str::<T>(&stripped) {
            return Ok(v);
        }
    }

    // Attempt 3: find the outermost JSON object substring.
    let located = locate_json_object(&stripped);
    serde_json::from_str::<T>(&located)
}

fn strip_code_fences(content: &str) -> String {
    let trimmed = content.trim();
    // Match ```<optional lang>\n ... \n```  or  ```<lang>\n ... ```  or  ``` ... ```
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip the language hint up to the first newline (if any).
        let after_lang = match rest.find('\n') {
            Some(nl) => &rest[nl + 1..],
            None => rest,
        };
        if let Some(without_trailing) = after_lang.strip_suffix("```") {
            return without_trailing.trim().to_string();
        }
        // No trailing fence — return what we have after the opening fence.
        return after_lang.trim().to_string();
    }
    trimmed.to_string()
}

fn locate_json_object(content: &str) -> String {
    let Some(start) = content.find('{') else {
        return content.to_string();
    };
    let Some(end) = content.rfind('}') else {
        return content.to_string();
    };
    if start >= end {
        return content.to_string();
    }
    content[start..=end].to_string()
}

// ==================== Utilities ====================

fn preview_of(content: &str) -> String {
    let mut out: String = content.chars().take(500).collect();
    if content.chars().count() > 500 {
        out.push_str("...[truncated]");
    }
    out
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::types::CompletionResponse;
    use async_trait::async_trait;
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Sample {
        value: String,
        count: i32,
    }

    // ==================== Mock provider ====================
    //
    // Programmable LlmProvider implementation for integration-level tests.
    // Tests enqueue a sequence of responses (successes or errors) and assert
    // on the sequence of calls + captured messages after driving the full
    // `call_structured_with_provider` pipeline.

    enum MockResponse {
        Ok(String),
        Err(ProviderError),
    }

    struct MockLlmProvider {
        responses: Mutex<VecDeque<MockResponse>>,
        call_count: AtomicUsize,
        captured_messages: Mutex<Vec<Vec<Message>>>,
        /// Captured `structured_output` field from each incoming config, so
        /// tests can assert on schema/strict/name propagation without cloning
        /// the full LlmConfig (which is borrow-heavy).
        captured_schemas: Mutex<Vec<Option<StructuredOutputSchema>>>,
    }

    impl MockLlmProvider {
        fn new() -> Self {
            Self {
                responses: Mutex::new(VecDeque::new()),
                call_count: AtomicUsize::new(0),
                captured_messages: Mutex::new(Vec::new()),
                captured_schemas: Mutex::new(Vec::new()),
            }
        }

        fn queue_response(&self, content: impl Into<String>) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .push_back(MockResponse::Ok(content.into()));
            self
        }

        fn queue_error(&self, error: ProviderError) -> &Self {
            self.responses
                .lock()
                .unwrap()
                .push_back(MockResponse::Err(error));
            self
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn captured_messages(&self) -> Vec<Vec<Message>> {
            self.captured_messages.lock().unwrap().clone()
        }

        fn captured_schemas(&self) -> Vec<Option<StructuredOutputSchema>> {
            self.captured_schemas
                .lock()
                .unwrap()
                .iter()
                .map(|s| {
                    s.as_ref().map(|schema| StructuredOutputSchema {
                        name: schema.name.clone(),
                        schema: schema.schema.clone(),
                        strict: schema.strict,
                    })
                })
                .collect()
        }
    }

    #[async_trait]
    impl LlmProvider for MockLlmProvider {
        async fn complete(
            &self,
            messages: &[Message],
            config: &LlmConfig,
        ) -> Result<CompletionResponse, ProviderError> {
            self.captured_messages
                .lock()
                .unwrap()
                .push(messages.to_vec());
            self.captured_schemas.lock().unwrap().push(
                config
                    .params
                    .structured_output
                    .as_ref()
                    .map(|s| StructuredOutputSchema {
                        name: s.name.clone(),
                        schema: s.schema.clone(),
                        strict: s.strict,
                    }),
            );
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match self.responses.lock().unwrap().pop_front() {
                Some(MockResponse::Ok(content)) => Ok(CompletionResponse {
                    content,
                    tool_calls: None,
                }),
                Some(MockResponse::Err(e)) => Err(e),
                None => Err(ProviderError::Configuration(
                    "mock response queue exhausted".to_string(),
                )),
            }
        }
    }

    fn sample_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" },
                "count": { "type": "integer" }
            },
            "required": ["value", "count"],
            "additionalProperties": false
        })
    }

    /// Minimal provider config for tests. None of the real provider lookups
    /// are hit because we bypass `get_llm_provider` via
    /// `call_structured_with_provider`.
    fn test_provider_config() -> ProviderConfig {
        ProviderConfig::from_settings(&std::collections::HashMap::new())
    }

    async fn run_sample(
        provider: Arc<MockLlmProvider>,
        messages: Vec<Message>,
    ) -> Result<Sample, StructuredCallError> {
        let config = test_provider_config();
        let call = StructuredCall::<Sample>::new(
            &config,
            "test-model",
            &messages,
            "sample_result",
            sample_schema(),
        )
        .with_max_retries(2);
        call_structured_with_provider::<Sample>(call, provider).await
    }

    // Minimal valid response matching `Sample`.
    const OK_JSON: &str = r#"{"value":"hello","count":7}"#;

    // ==================== Pipeline integration tests ====================
    //
    // These exercise the full call_structured_with_provider loop:
    // primary → parse → (retry | fallback) → parse → Ok | Err. Each test
    // programs a sequence of mock responses and then asserts on both the
    // returned value AND the number/shape of provider calls, so they catch
    // regressions in the orchestration logic that parse/lint unit tests
    // can't see.

    #[tokio::test]
    async fn pipeline_primary_success_no_fallback() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response(OK_JSON);

        let messages = vec![Message::system("s"), Message::user("u")];
        let result = run_sample(provider.clone(), messages).await.unwrap();

        assert_eq!(
            result,
            Sample {
                value: "hello".into(),
                count: 7
            }
        );
        assert_eq!(provider.call_count(), 1, "should not have fired fallback");
    }

    #[tokio::test]
    async fn pipeline_fenced_json_parses_without_fallback() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response("```json\n{\"value\":\"hi\",\"count\":1}\n```");

        let messages = vec![Message::system("s"), Message::user("u")];
        let result = run_sample(provider.clone(), messages).await.unwrap();

        assert_eq!(result.count, 1);
        // Tolerant parse handled the fence — no fallback call needed.
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn pipeline_prose_wrapped_json_parses_without_fallback() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response("Sure, here's the data: {\"value\":\"hi\",\"count\":2}\n\nLet me know if you need more.");

        let messages = vec![Message::system("s"), Message::user("u")];
        let result = run_sample(provider.clone(), messages).await.unwrap();

        assert_eq!(result.count, 2);
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn pipeline_prose_response_triggers_fallback() {
        let provider = Arc::new(MockLlmProvider::new());
        // Primary: completely unparseable prose
        provider.queue_response("I'm unable to provide that in JSON format.");
        // Fallback: properly formatted JSON
        provider.queue_response(OK_JSON);

        let messages = vec![Message::system("s"), Message::user("u")];
        let result = run_sample(provider.clone(), messages).await.unwrap();

        assert_eq!(result.count, 7);
        assert_eq!(provider.call_count(), 2, "fallback should have fired");

        // Verify the fallback call included the schema nudge as a user message
        let captured = provider.captured_messages();
        assert_eq!(captured.len(), 2);
        let fallback_msgs = &captured[1];
        // Fallback message count = original messages + nudge
        assert_eq!(fallback_msgs.len(), 3);
        let last = &fallback_msgs[fallback_msgs.len() - 1];
        assert!(matches!(
            last.role,
            crate::providers::types::MessageRole::User
        ));
        let nudge = last.content.as_deref().unwrap_or("");
        assert!(
            nudge.contains("could not be parsed") && nudge.contains("Schema:"),
            "nudge should describe the parse failure and include the schema"
        );
    }

    #[tokio::test]
    async fn pipeline_both_paths_fail_returns_parse_failed() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response("prose prose prose");
        provider.queue_response("still prose after the nudge");

        let messages = vec![Message::system("s"), Message::user("u")];
        let err = run_sample(provider.clone(), messages).await.unwrap_err();

        assert_eq!(provider.call_count(), 2);
        match err {
            StructuredCallError::ParseFailed {
                attempts, preview, ..
            } => {
                assert_eq!(attempts, 2);
                assert!(
                    preview.contains("still prose"),
                    "preview should be the fallback response"
                );
            }
            other => panic!("expected ParseFailed, got {:?}", other),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_transient_error_retries_then_succeeds() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_error(ProviderError::RateLimited {
            retry_after_secs: None,
        });
        provider.queue_response(OK_JSON);

        let messages = vec![Message::system("s"), Message::user("u")];
        let result = run_sample(provider.clone(), messages).await.unwrap();

        assert_eq!(result.count, 7);
        assert_eq!(provider.call_count(), 2, "retry consumed 1 extra call");
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_retry_exhausted_returns_provider_error() {
        let provider = Arc::new(MockLlmProvider::new());
        // max_retries = 2 → 1 primary + 2 retries = 3 failing calls before
        // the loop gives up. All transient so the loop keeps trying.
        provider.queue_error(ProviderError::Network("t1".into()));
        provider.queue_error(ProviderError::Network("t2".into()));
        provider.queue_error(ProviderError::Network("t3".into()));

        let messages = vec![Message::system("s"), Message::user("u")];
        let err = run_sample(provider.clone(), messages).await.unwrap_err();

        assert_eq!(provider.call_count(), 3);
        assert!(matches!(
            err,
            StructuredCallError::Provider(ProviderError::Network(_))
        ));
    }

    #[tokio::test]
    async fn pipeline_non_retryable_error_bails_immediately() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_error(ProviderError::Api {
            status: 400,
            message: "bad request".into(),
        });

        let messages = vec![Message::system("s"), Message::user("u")];
        let err = run_sample(provider.clone(), messages).await.unwrap_err();

        // 400 is non-retryable (see ProviderError::is_retryable), so we bail
        // on the first call without consuming retries.
        assert_eq!(provider.call_count(), 1);
        assert!(matches!(
            err,
            StructuredCallError::Provider(ProviderError::Api { status: 400, .. })
        ));
    }

    #[tokio::test]
    async fn pipeline_fallback_provider_call_also_fails() {
        let provider = Arc::new(MockLlmProvider::new());
        // Primary: unparseable prose
        provider.queue_response("nope, not doing JSON today");
        // Fallback: provider itself errors out
        provider.queue_error(ProviderError::Api {
            status: 500,
            message: "upstream exploded".into(),
        });

        let messages = vec![Message::system("s"), Message::user("u")];
        let err = run_sample(provider.clone(), messages).await.unwrap_err();

        assert_eq!(provider.call_count(), 2);
        match err {
            StructuredCallError::ParseFailed { parse_error, .. } => {
                // Primary parse failure is the primary diagnostic, but the
                // error text should mention the fallback also failed.
                assert!(
                    parse_error.contains("fallback"),
                    "error should surface the fallback failure: {}",
                    parse_error
                );
            }
            other => panic!("expected ParseFailed, got {:?}", other),
        }
    }

    // ==================== Strict-mode escape hatch ====================

    #[test]
    fn structured_call_default_strict_is_true() {
        let config = test_provider_config();
        let messages: Vec<Message> = vec![];
        let call = StructuredCall::<Sample>::new(
            &config,
            "m",
            &messages,
            "sample_result",
            sample_schema(),
        );
        assert!(call.strict, "default strict must be true");
    }

    #[test]
    fn with_strict_false_sets_field() {
        let config = test_provider_config();
        let messages: Vec<Message> = vec![];
        let call = StructuredCall::<Sample>::new(
            &config,
            "m",
            &messages,
            "sample_result",
            sample_schema(),
        )
        .with_strict(false);
        assert!(!call.strict);
    }

    #[tokio::test]
    async fn pipeline_primary_call_sends_strict_true_by_default() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response(OK_JSON);

        let messages = vec![Message::user("u")];
        run_sample(provider.clone(), messages).await.unwrap();

        let schemas = provider.captured_schemas();
        assert_eq!(schemas.len(), 1, "only primary call, no fallback");
        let primary = schemas[0].as_ref().expect("primary must carry a schema");
        assert_eq!(primary.name, "sample_result");
        assert!(primary.strict, "primary call must default to strict=true");
    }

    #[tokio::test]
    async fn pipeline_primary_call_sends_strict_false_when_opted_out() {
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response(OK_JSON);

        let config = test_provider_config();
        let messages = vec![Message::user("u")];
        let call = StructuredCall::<Sample>::new(
            &config,
            "weak-model",
            &messages,
            "sample_result",
            sample_schema(),
        )
        .with_strict(false);
        call_structured_with_provider::<Sample>(call, provider.clone())
            .await
            .unwrap();

        let schemas = provider.captured_schemas();
        assert_eq!(schemas.len(), 1);
        let primary = schemas[0].as_ref().expect("primary must carry a schema");
        assert!(
            !primary.strict,
            "with_strict(false) must propagate to the outbound request"
        );
    }

    #[tokio::test]
    async fn pipeline_fallback_call_never_sends_schema() {
        // When the primary response can't be parsed we fire a fallback call.
        // That call should NOT carry `structured_output` — the nudge prompt
        // is our only mechanism on the fallback path, and some providers
        // reject repeated schema attempts after an initial refusal.
        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response("prose, no json here");
        provider.queue_response(OK_JSON);

        let messages = vec![Message::user("u")];
        run_sample(provider.clone(), messages).await.unwrap();

        let schemas = provider.captured_schemas();
        assert_eq!(schemas.len(), 2, "primary + fallback");
        assert!(schemas[0].is_some(), "primary must carry the schema");
        assert!(
            schemas[1].is_none(),
            "fallback call must NOT carry structured_output"
        );
    }

    #[tokio::test]
    async fn pipeline_schema_lint_fails_before_any_call() {
        // Schema with oneOf — lint rejects, we never reach the provider.
        let bad_schema = json!({
            "type": "object",
            "properties": {
                "value": { "oneOf": [{"type": "string"}] }
            },
            "required": ["value"],
            "additionalProperties": false
        });

        let provider = Arc::new(MockLlmProvider::new());
        provider.queue_response(OK_JSON);

        let config = test_provider_config();
        let messages = vec![Message::user("u")];
        let call = StructuredCall::<Sample>::new(
            &config,
            "test-model",
            &messages,
            "sample_result",
            bad_schema,
        );

        let err = call_structured_with_provider::<Sample>(call, provider.clone())
            .await
            .unwrap_err();

        assert_eq!(
            provider.call_count(),
            0,
            "should not call provider on lint failure"
        );
        assert!(matches!(err, StructuredCallError::SchemaLint(_)));
    }

    // ==================== Schema snapshot ====================
    //
    // Golden file of every live schema passed to `call_structured`. The
    // snapshot catches semantic drift that the lint tests can't see — e.g.
    // someone renaming `article_content` to `content`, or flipping an
    // integer field to a string. The lint tests still pass but real LLM
    // calls start returning data that deserializes into the wrong shape.
    //
    // Regenerate with:
    //   UPDATE_SNAPSHOTS=1 cargo test -p atomic-core schema_snapshot
    //
    // The test collects schemas from every module that uses structured
    // output. If you add a new `call_structured` call site, add its schema
    // here and rerun with UPDATE_SNAPSHOTS=1 to bake it into the snapshot.

    fn collect_live_schemas() -> std::collections::BTreeMap<&'static str, Value> {
        let mut out = std::collections::BTreeMap::new();
        out.insert(
            "wiki_generation_result",
            crate::wiki::wiki_generation_schema(),
        );
        out.insert("wiki_update_section_ops", crate::wiki::section_ops_schema());
        out.insert(
            "report_finding_result",
            crate::reports::agentic::report_schema_for_snapshot(),
        );
        out.insert("extraction_result", crate::extraction::extraction_schema());
        out.insert(
            "consolidation_result",
            crate::extraction::consolidation_schema(),
        );
        out.insert("merge_result", crate::compaction::merge_schema());
        out
    }

    fn snapshot_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/providers/testdata/schemas.snap.json")
    }

    // serde_json::Map's backing store is BTreeMap by default but flips to
    // IndexMap when any crate in the graph enables serde_json's
    // `preserve_order` feature (utoipa does, so an atomic-server workspace
    // build unifies it on). That means we can't rely on serialization order
    // — canonicalize by recursively sorting object keys before comparing.
    fn canonicalize(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), canonicalize(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
            _ => v.clone(),
        }
    }

    #[test]
    fn schema_snapshot_matches_live_schemas() {
        let live = collect_live_schemas();
        let canonical: std::collections::BTreeMap<&'static str, Value> =
            live.iter().map(|(k, v)| (*k, canonicalize(v))).collect();
        let current =
            serde_json::to_string_pretty(&canonical).expect("schema snapshot should serialize");

        let path = snapshot_path();

        if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{}\n", current)).expect("failed to write snapshot");
            eprintln!("[snapshot] wrote {}", path.display());
            return;
        }

        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "Snapshot file missing at {}: {}\n\n\
                 Run: UPDATE_SNAPSHOTS=1 cargo test -p atomic-core schema_snapshot",
                path.display(),
                e
            )
        });

        let expected_trimmed = expected.trim_end();
        let current_trimmed = current.trim_end();

        if expected_trimmed != current_trimmed {
            // Compute which top-level schema names drifted so the error
            // points at the offending caller quickly.
            let expected_parsed: serde_json::Map<String, Value> =
                serde_json::from_str(expected_trimmed)
                    .expect("existing snapshot should be valid JSON");
            let current_parsed: serde_json::Map<String, Value> =
                serde_json::from_str(current_trimmed).unwrap();

            let mut drifted: Vec<String> = Vec::new();
            for (k, v) in &current_parsed {
                match expected_parsed.get(k) {
                    Some(existing) if existing == v => {}
                    Some(_) => drifted.push(format!("{} (changed)", k)),
                    None => drifted.push(format!("{} (added)", k)),
                }
            }
            for k in expected_parsed.keys() {
                if !current_parsed.contains_key(k) {
                    drifted.push(format!("{} (removed)", k));
                }
            }

            panic!(
                "Schema snapshot mismatch in: {}\n\n\
                 To regenerate the snapshot:\n\
                     UPDATE_SNAPSHOTS=1 cargo test -p atomic-core schema_snapshot\n",
                drifted.join(", ")
            );
        }
    }

    #[test]
    fn every_live_schema_is_lint_clean() {
        // Belt-and-suspenders: the per-module lint tests already cover each
        // schema individually, but this iterates the full registry so a
        // newly-added schema is automatically included the moment the
        // author registers it in `collect_live_schemas`.
        for (name, schema) in collect_live_schemas() {
            lint_schema(&schema).unwrap_or_else(|e| panic!("schema '{}' failed lint: {}", name, e));
        }
    }

    // ---------- parse_tolerant ----------

    #[test]
    fn parse_raw_json() {
        let input = r#"{"value":"hello","count":3}"#;
        let parsed: Sample = parse_tolerant(input).unwrap();
        assert_eq!(
            parsed,
            Sample {
                value: "hello".into(),
                count: 3
            }
        );
    }

    #[test]
    fn parse_fenced_json_block() {
        let input = "```json\n{\"value\":\"hi\",\"count\":1}\n```";
        let parsed: Sample = parse_tolerant(input).unwrap();
        assert_eq!(
            parsed,
            Sample {
                value: "hi".into(),
                count: 1
            }
        );
    }

    #[test]
    fn parse_fenced_generic_block() {
        let input = "```\n{\"value\":\"hi\",\"count\":2}\n```";
        let parsed: Sample = parse_tolerant(input).unwrap();
        assert_eq!(
            parsed,
            Sample {
                value: "hi".into(),
                count: 2
            }
        );
    }

    #[test]
    fn parse_json_with_prose_prefix() {
        let input = "Sure! Here is the JSON you asked for: {\"value\":\"ok\",\"count\":7}";
        let parsed: Sample = parse_tolerant(input).unwrap();
        assert_eq!(
            parsed,
            Sample {
                value: "ok".into(),
                count: 7
            }
        );
    }

    #[test]
    fn parse_json_with_trailing_prose() {
        let input = "{\"value\":\"x\",\"count\":0}\n\nLet me know if you need anything else!";
        let parsed: Sample = parse_tolerant(input).unwrap();
        assert_eq!(
            parsed,
            Sample {
                value: "x".into(),
                count: 0
            }
        );
    }

    #[test]
    fn parse_garbage_fails() {
        let input = "not json at all";
        let result: Result<Sample, _> = parse_tolerant(input);
        assert!(result.is_err());
    }

    // ---------- lint_schema ----------

    fn portable_sample_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" },
                "count": { "type": "integer" }
            },
            "required": ["value", "count"],
            "additionalProperties": false
        })
    }

    #[test]
    fn lint_accepts_portable_schema() {
        assert!(lint_schema(&portable_sample_schema()).is_ok());
    }

    #[test]
    fn lint_rejects_one_of() {
        let schema = json!({
            "type": "object",
            "properties": {
                "op": {
                    "oneOf": [
                        { "type": "object", "properties": { "kind": { "type": "string" } }, "required": ["kind"], "additionalProperties": false }
                    ]
                }
            },
            "required": ["op"],
            "additionalProperties": false
        });
        let err = lint_schema(&schema).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("oneOf"),
            "expected oneOf violation, got: {}",
            msg
        );
    }

    #[test]
    fn lint_rejects_nullable_union() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": { "type": ["string", "null"] }
            },
            "required": ["value"],
            "additionalProperties": false
        });
        let err = lint_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("nullable"));
    }

    #[test]
    fn lint_rejects_missing_additional_properties_false() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            },
            "required": ["value"]
        });
        let err = lint_schema(&schema).unwrap_err();
        assert!(err.to_string().contains("additionalProperties"));
    }

    #[test]
    fn lint_rejects_optional_property() {
        let schema = json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" },
                "count": { "type": "integer" }
            },
            "required": ["value"],
            "additionalProperties": false
        });
        let err = lint_schema(&schema).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("count") && msg.contains("required"),
            "expected missing-required violation for 'count', got: {}",
            msg
        );
    }

    #[test]
    fn lint_accepts_nested_portable_schema() {
        // Mirrors the shape wiki section_ops_schema uses.
        let schema = json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": { "type": "string", "enum": ["NoChange", "Append"] },
                            "content": { "type": "string" }
                        },
                        "required": ["op", "content"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["operations"],
            "additionalProperties": false
        });
        assert!(lint_schema(&schema).is_ok());
    }

    // ---------- strip_code_fences ----------

    #[test]
    fn strip_fences_json_lang() {
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_no_lang() {
        assert_eq!(strip_code_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_fences_noop() {
        assert_eq!(strip_code_fences("{\"a\":1}"), "{\"a\":1}");
    }
}
