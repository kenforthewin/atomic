//! OpenRouter LLM implementation

use crate::providers::error::ProviderError;
use crate::providers::openrouter::OpenRouterProvider;
use crate::providers::traits::{LlmConfig, StreamCallback};
use crate::providers::types::{
    CompletionResponse, Message, StreamDelta, ToolCall, ToolCallFunction, ToolDefinition,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

// ==================== Request Types ====================

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderPreferences>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    stream: bool,
}

#[derive(Serialize)]
struct ReasoningConfig {
    effort: String,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct ApiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: ApiFunctionDef,
}

#[derive(Serialize)]
struct ApiFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize, Clone)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ApiFunctionCall,
}

#[derive(Serialize, Clone)]
struct ApiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<JsonSchemaWrapper>,
}

#[derive(Serialize)]
struct JsonSchemaWrapper {
    name: String,
    strict: bool,
    schema: serde_json::Value,
}

#[derive(Serialize)]
struct ProviderPreferences {
    require_parameters: bool,
}

// ==================== Response Types ====================

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Deserialize, Clone)]
struct ResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ResponseFunctionCall,
}

#[derive(Deserialize, Clone)]
struct ResponseFunctionCall {
    name: String,
    arguments: String,
}

// ==================== Streaming Types ====================

#[derive(Deserialize)]
struct StreamingResponse {
    choices: Vec<StreamingChoice>,
}

#[derive(Deserialize)]
struct StreamingChoice {
    delta: StreamingDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct StreamingDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamingToolCall>>,
}

#[derive(Deserialize)]
struct StreamingToolCall {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    call_type: Option<String>,
    function: Option<StreamingFunction>,
}

#[derive(Deserialize)]
struct StreamingFunction {
    name: Option<String>,
    arguments: Option<String>,
}

/// Accumulator for building complete tool calls from streaming deltas
#[derive(Default, Clone)]
struct ToolCallAccumulator {
    id: String,
    call_type: String,
    name: String,
    arguments: String,
}

// ==================== Conversion Functions ====================

fn convert_message(msg: &Message) -> ApiMessage {
    ApiMessage {
        role: msg.role.as_str().to_string(),
        content: msg.content.clone(),
        tool_calls: msg.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| ApiToolCall {
                    id: tc.id.clone(),
                    call_type: tc
                        .call_type
                        .clone()
                        .unwrap_or_else(|| "function".to_string()),
                    function: ApiFunctionCall {
                        name: tc.get_name().unwrap_or_default().to_string(),
                        arguments: tc.get_arguments().unwrap_or_default().to_string(),
                    },
                })
                .collect()
        }),
        tool_call_id: msg.tool_call_id.clone(),
        name: msg.name.clone(),
    }
}

fn convert_tool(tool: &ToolDefinition) -> ApiTool {
    ApiTool {
        tool_type: "function".to_string(),
        function: ApiFunctionDef {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
        },
    }
}

fn convert_tool_call(tc: &ResponseToolCall) -> ToolCall {
    ToolCall {
        id: tc.id.clone(),
        call_type: Some(tc.call_type.clone()),
        function: Some(ToolCallFunction {
            name: tc.function.name.clone(),
            arguments: tc.function.arguments.clone(),
        }),
        name: None,
        arguments: None,
    }
}

// ==================== Non-Streaming Implementation ====================

pub async fn complete(
    provider: &OpenRouterProvider,
    messages: &[Message],
    config: &LlmConfig,
) -> Result<CompletionResponse, ProviderError> {
    complete_internal(provider, messages, &[], config, false).await
}

pub async fn complete_with_tools(
    provider: &OpenRouterProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
) -> Result<CompletionResponse, ProviderError> {
    complete_internal(provider, messages, tools, config, false).await
}

async fn complete_internal(
    provider: &OpenRouterProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
    _stream: bool,
) -> Result<CompletionResponse, ProviderError> {
    let api_messages: Vec<ApiMessage> = messages.iter().map(convert_message).collect();
    let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
        None
    } else {
        Some(tools.iter().map(convert_tool).collect())
    };

    // Build response format if structured output is requested
    let response_format = config
        .params
        .structured_output
        .as_ref()
        .map(|schema| ResponseFormat {
            format_type: "json_schema".to_string(),
            json_schema: Some(JsonSchemaWrapper {
                name: schema.name.clone(),
                strict: schema.strict,
                schema: schema.schema.clone(),
            }),
        });

    let provider_prefs = if config.params.structured_output.is_some() {
        Some(ProviderPreferences {
            require_parameters: true,
        })
    } else {
        None
    };

    // Filter parameters based on model support. Under `require_parameters`
    // (structured output), sampling preferences ride ONLY when positively
    // known supported: OpenRouter routes strict requests exclusively to
    // endpoints supporting every sent parameter, so an assumed-universal
    // param (temperature on the GPT-5 reasoning family) yields a routing 404
    // rather than being ignored. A preference must never constrain routing.
    let strict_routing = provider_prefs.is_some();
    let param_ok = |name: &str| {
        if strict_routing {
            config.params.is_param_known_supported(name)
        } else {
            config.params.is_param_supported(name)
        }
    };

    let temperature = if param_ok("temperature") {
        config.params.temperature
    } else {
        None
    };

    // max_tokens is exempt from the strict-routing filter: it's universal
    // across the OpenAI-compatible surface, and DROPPING it is the harmful
    // direction — Anthropic's API requires the field, so an absent value
    // gets a router-filled small default and long outputs come back
    // truncated mid-sentence (which is exactly what happened when strict
    // filtering stripped it here: capabilities are rarely loaded on
    // background paths, so `is_param_known_supported` said no to
    // everything). Temperature stays filtered — it's the param that
    // empties the endpoint pool into a routing 404 when over-sent.
    let max_tokens = config.params.max_tokens;

    // Only minimize reasoning when explicitly requested (for simple tasks like tag extraction)
    let reasoning =
        if config.params.minimize_reasoning && config.params.is_param_supported("reasoning") {
            Some(ReasoningConfig {
                effort: "minimal".to_string(),
            })
        } else {
            None
        };

    let request = ChatRequest {
        model: config.model.clone(),
        messages: api_messages,
        tools: api_tools,
        tool_choice: None,
        temperature,
        max_tokens,
        response_format,
        provider: provider_prefs,
        reasoning,
        stream: false,
    };

    let response = provider
        .client()
        .post(format!("{}/chat/completions", provider.base_url()))
        .header("Authorization", format!("Bearer {}", provider.api_key()))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://atomic.app")
        .header("X-Title", "Atomic")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let body = response.text().await.unwrap_or_default();

        if status == 429 {
            tracing::warn!(status, retry_after, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 200), "OpenRouter LLM rate limited");
            return Err(ProviderError::RateLimited {
                retry_after_secs: retry_after,
            });
        }

        tracing::error!(status, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 500), "OpenRouter LLM API error");
        return Err(ProviderError::Api {
            status,
            message: body,
        });
    }

    let body = response.text().await?;
    let chat_response: ChatResponse = serde_json::from_str(&body)
        .map_err(|e| {
            tracing::error!(error = %e, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 500), "OpenRouter LLM parse error");
            ProviderError::ParseError(format!("Failed to parse chat response: {e}"))
        })?;

    let choice = chat_response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ProviderError::ParseError("No choices in response".to_string()))?;

    let tool_calls = choice
        .message
        .tool_calls
        .map(|tcs| tcs.iter().map(convert_tool_call).collect());

    Ok(CompletionResponse {
        content: choice.message.content.unwrap_or_default(),
        tool_calls,
    })
}

// ==================== Streaming Implementation ====================

pub async fn complete_streaming_with_tools(
    provider: &OpenRouterProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
    on_delta: StreamCallback,
) -> Result<CompletionResponse, ProviderError> {
    complete_streaming_internal(provider, messages, tools, config, on_delta).await
}

async fn complete_streaming_internal(
    provider: &OpenRouterProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
    on_delta: StreamCallback,
) -> Result<CompletionResponse, ProviderError> {
    let api_messages: Vec<ApiMessage> = messages.iter().map(convert_message).collect();
    let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
        None
    } else {
        Some(tools.iter().map(convert_tool).collect())
    };

    // Only minimize reasoning when explicitly requested
    let reasoning =
        if config.params.minimize_reasoning && config.params.is_param_supported("reasoning") {
            Some(ReasoningConfig {
                effort: "minimal".to_string(),
            })
        } else {
            None
        };

    let request = ChatRequest {
        model: config.model.clone(),
        messages: api_messages,
        tools: api_tools,
        tool_choice: None,
        temperature: config.params.temperature,
        max_tokens: config.params.max_tokens,
        response_format: None, // Streaming doesn't support structured output
        provider: None,
        reasoning,
        stream: true,
    };

    let response = provider
        .client()
        .post(format!("{}/chat/completions", provider.base_url()))
        .header("Authorization", format!("Bearer {}", provider.api_key()))
        .header("Content-Type", "application/json")
        .header("HTTP-Referer", "https://atomic.app")
        .header("X-Title", "Atomic")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let body = response.text().await.unwrap_or_default();

        if status == 429 {
            tracing::warn!(status, retry_after, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 200), "OpenRouter streaming LLM rate limited");
            return Err(ProviderError::RateLimited {
                retry_after_secs: retry_after,
            });
        }

        tracing::error!(status, model = %config.model, body_preview = %crate::providers::error::truncate_utf8(&body, 500), "OpenRouter streaming LLM API error");
        return Err(ProviderError::Api {
            status,
            message: body,
        });
    }

    // Process the streaming response
    let mut content = String::new();
    let mut tool_call_accumulators: Vec<ToolCallAccumulator> = Vec::new();
    let mut buffer = String::new();
    let mut finish_reason = None;

    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| ProviderError::Network(e.to_string()))?;
        let chunk_str = String::from_utf8_lossy(&chunk);
        buffer.push_str(&chunk_str);

        // Process complete lines from buffer
        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim().to_string();
            buffer = buffer[line_end + 1..].to_string();

            // Skip empty lines
            if line.is_empty() {
                continue;
            }

            // Check for stream end
            if line == "data: [DONE]" {
                on_delta(StreamDelta::Done {
                    finish_reason: finish_reason.clone(),
                });
                break;
            }

            // Parse SSE data line
            if let Some(json_str) = line.strip_prefix("data: ") {
                match serde_json::from_str::<StreamingResponse>(json_str) {
                    Err(e) => {
                        tracing::debug!(error = %e, chunk_preview = %crate::providers::error::truncate_utf8(json_str, 200), "OpenRouter stream chunk parse error");
                    }
                    Ok(response) => {
                        if let Some(choice) = response.choices.first() {
                            // Update finish reason
                            if choice.finish_reason.is_some() {
                                finish_reason = choice.finish_reason.clone();
                            }

                            // Handle content delta
                            if let Some(delta_content) = &choice.delta.content {
                                content.push_str(delta_content);
                                on_delta(StreamDelta::Content(delta_content.clone()));
                            }

                            // Handle tool call deltas
                            if let Some(tool_calls) = &choice.delta.tool_calls {
                                for tc in tool_calls {
                                    // Ensure accumulator exists for this index
                                    while tool_call_accumulators.len() <= tc.index {
                                        tool_call_accumulators.push(ToolCallAccumulator::default());
                                    }

                                    let acc = &mut tool_call_accumulators[tc.index];
                                    let mut name_changed = false;

                                    // Accumulate fields
                                    if let Some(id) = &tc.id {
                                        acc.id = id.clone();
                                    }
                                    if let Some(call_type) = &tc.call_type {
                                        acc.call_type = call_type.clone();
                                    }
                                    if let Some(func) = &tc.function {
                                        if let Some(name) = &func.name {
                                            if acc.name.is_empty() {
                                                acc.name = name.clone();
                                                name_changed = true;
                                            }
                                        }
                                        if let Some(args) = &func.arguments {
                                            acc.arguments.push_str(args);
                                            // Emit argument delta
                                            on_delta(StreamDelta::ToolCallArguments {
                                                index: tc.index,
                                                arguments: args.clone(),
                                            });
                                        }
                                    }

                                    // Emit tool call start when we have both id and name
                                    if name_changed && !acc.id.is_empty() && !acc.name.is_empty() {
                                        on_delta(StreamDelta::ToolCallStart {
                                            index: tc.index,
                                            id: acc.id.clone(),
                                            name: acc.name.clone(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Convert accumulators to ToolCall
    let tool_calls = if tool_call_accumulators.is_empty() {
        None
    } else {
        Some(
            tool_call_accumulators
                .into_iter()
                .map(|acc| ToolCall {
                    id: acc.id,
                    call_type: Some(acc.call_type),
                    function: Some(ToolCallFunction {
                        name: acc.name,
                        arguments: acc.arguments,
                    }),
                    name: None,
                    arguments: None,
                })
                .collect(),
        )
    };

    Ok(CompletionResponse {
        content,
        tool_calls,
    })
}
