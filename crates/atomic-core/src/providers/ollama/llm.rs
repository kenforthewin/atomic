//! Ollama LLM implementation

use crate::providers::error::ProviderError;
use crate::providers::ollama::OllamaProvider;
use crate::providers::traits::{LlmConfig, StreamCallback};
use crate::providers::types::{
    CompletionResponse, Message, MessageRole, StreamDelta, ToolCall, ToolCallFunction,
    ToolDefinition,
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
    format: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<ChatOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
    stream: bool,
}

#[derive(Serialize)]
struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
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
    function: ApiFunctionCall,
}

#[derive(Serialize, Clone)]
struct ApiFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

// ==================== Response Types ====================

#[derive(Deserialize)]
struct ChatResponse {
    message: ResponseMessage,
    #[allow(dead_code)]
    done: bool,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Deserialize, Clone)]
struct ResponseToolCall {
    function: ResponseFunctionCall,
}

#[derive(Deserialize, Clone)]
struct ResponseFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

// ==================== Streaming Types ====================

#[derive(Deserialize)]
struct StreamingResponse {
    message: StreamingMessage,
    done: bool,
}

#[derive(Deserialize, Default)]
struct StreamingMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<ResponseToolCall>>,
}

// ==================== Conversion Functions ====================

fn convert_message(msg: &Message) -> ApiMessage {
    let role = match msg.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    ApiMessage {
        role: role.to_string(),
        content: msg.content.clone().unwrap_or_default(),
        tool_calls: msg.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| {
                    let args_str = tc.get_arguments().unwrap_or("{}");
                    let args: serde_json::Value =
                        serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                    ApiToolCall {
                        function: ApiFunctionCall {
                            name: tc.get_name().unwrap_or_default().to_string(),
                            arguments: args,
                        },
                    }
                })
                .collect()
        }),
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

fn convert_tool_call(tc: &ResponseToolCall, _index: usize) -> ToolCall {
    // Ollama returns arguments as parsed JSON, we need to stringify it
    let arguments = serde_json::to_string(&tc.function.arguments).unwrap_or_default();

    ToolCall {
        id: format!("call_{}", uuid::Uuid::new_v4()),
        call_type: Some("function".to_string()),
        function: Some(ToolCallFunction {
            name: tc.function.name.clone(),
            arguments,
        }),
        name: None,
        arguments: None,
    }
}

// ==================== Non-Streaming Implementation ====================

pub async fn complete(
    provider: &OllamaProvider,
    messages: &[Message],
    config: &LlmConfig,
) -> Result<CompletionResponse, ProviderError> {
    let api_messages: Vec<ApiMessage> = messages.iter().map(convert_message).collect();

    // Build format if structured output is requested
    let format = config
        .params
        .structured_output
        .as_ref()
        .map(|schema| schema.schema.clone());

    let options = if config.params.temperature.is_some() || config.params.max_tokens.is_some() {
        Some(ChatOptions {
            temperature: config.params.temperature,
            num_predict: config.params.max_tokens,
        })
    } else {
        None
    };

    // Disable thinking for faster responses when minimize_reasoning is true
    let think = if config.params.minimize_reasoning {
        Some(false)
    } else {
        None
    };

    let request = ChatRequest {
        model: config.model.clone(),
        messages: api_messages,
        tools: None,
        format,
        options,
        think,
        stream: false,
    };

    let response = provider
        .client()
        .post(format!("{}/api/chat", provider.base_url()))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        return Err(ProviderError::Api {
            status,
            message: body,
        });
    }

    let chat_response: ChatResponse = response.json().await?;

    let tool_calls = chat_response.message.tool_calls.map(|tcs| {
        tcs.iter()
            .enumerate()
            .map(|(i, tc)| convert_tool_call(tc, i))
            .collect()
    });

    Ok(CompletionResponse {
        content: chat_response.message.content,
        tool_calls,
        finish_reason: None,
        native_finish_reason: None,
        completion_tokens: None,
        upstream_provider: None,
    })
}

pub async fn complete_with_tools(
    provider: &OllamaProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    config: &LlmConfig,
) -> Result<CompletionResponse, ProviderError> {
    let api_messages: Vec<ApiMessage> = messages.iter().map(convert_message).collect();
    let api_tools: Option<Vec<ApiTool>> = if tools.is_empty() {
        None
    } else {
        Some(tools.iter().map(convert_tool).collect())
    };

    let format = config
        .params
        .structured_output
        .as_ref()
        .map(|schema| schema.schema.clone());

    let options = if config.params.temperature.is_some() || config.params.max_tokens.is_some() {
        Some(ChatOptions {
            temperature: config.params.temperature,
            num_predict: config.params.max_tokens,
        })
    } else {
        None
    };

    // Disable thinking for faster responses when minimize_reasoning is true
    let think = if config.params.minimize_reasoning {
        Some(false)
    } else {
        None
    };

    let request = ChatRequest {
        model: config.model.clone(),
        messages: api_messages,
        tools: api_tools,
        format,
        options,
        think,
        stream: false,
    };

    let response = provider
        .client()
        .post(format!("{}/api/chat", provider.base_url()))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        return Err(ProviderError::Api {
            status,
            message: body,
        });
    }

    let chat_response: ChatResponse = response.json().await?;

    let tool_calls = chat_response.message.tool_calls.map(|tcs| {
        tcs.iter()
            .enumerate()
            .map(|(i, tc)| convert_tool_call(tc, i))
            .collect()
    });

    Ok(CompletionResponse {
        content: chat_response.message.content,
        tool_calls,
        finish_reason: None,
        native_finish_reason: None,
        completion_tokens: None,
        upstream_provider: None,
    })
}

// ==================== Streaming Implementation ====================

pub async fn complete_streaming_with_tools(
    provider: &OllamaProvider,
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

    let options = if config.params.temperature.is_some() || config.params.max_tokens.is_some() {
        Some(ChatOptions {
            temperature: config.params.temperature,
            num_predict: config.params.max_tokens,
        })
    } else {
        None
    };

    // Disable thinking for faster responses when minimize_reasoning is true
    let think = if config.params.minimize_reasoning {
        Some(false)
    } else {
        None
    };

    let request = ChatRequest {
        model: config.model.clone(),
        messages: api_messages,
        tools: api_tools,
        format: None, // Streaming doesn't support structured output
        options,
        think,
        stream: true,
    };

    let response = provider
        .client()
        .post(format!("{}/api/chat", provider.base_url()))
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response.text().await.unwrap_or_default();

        return Err(ProviderError::Api {
            status,
            message: body,
        });
    }

    // Process the NDJSON streaming response
    let mut content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut buffer = String::new();

    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| ProviderError::Network(e.to_string()))?;
        let chunk_str = String::from_utf8_lossy(&chunk);
        buffer.push_str(&chunk_str);

        // Process complete lines from buffer (NDJSON format)
        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim().to_string();
            buffer = buffer[line_end + 1..].to_string();

            // Skip empty lines
            if line.is_empty() {
                continue;
            }

            // Parse the JSON line
            if let Ok(response) = serde_json::from_str::<StreamingResponse>(&line) {
                // Handle content delta
                if !response.message.content.is_empty() {
                    content.push_str(&response.message.content);
                    on_delta(StreamDelta::Content(response.message.content.clone()));
                }

                // Handle tool calls - they typically come all at once in Ollama
                if let Some(tcs) = response.message.tool_calls {
                    for (i, tc) in tcs.iter().enumerate() {
                        let tool_call = convert_tool_call(tc, tool_calls.len() + i);

                        // Emit tool call start
                        on_delta(StreamDelta::ToolCallStart {
                            index: tool_calls.len() + i,
                            id: tool_call.id.clone(),
                            name: tc.function.name.clone(),
                        });

                        // Emit tool call arguments
                        let args =
                            serde_json::to_string(&tc.function.arguments).unwrap_or_default();
                        on_delta(StreamDelta::ToolCallArguments {
                            index: tool_calls.len() + i,
                            arguments: args,
                        });

                        tool_calls.push(tool_call);
                    }
                }

                // Check if done
                if response.done {
                    on_delta(StreamDelta::Done {
                        finish_reason: Some("stop".to_string()),
                    });
                    break;
                }
            }
        }
    }

    Ok(CompletionResponse {
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        finish_reason: None,
        native_finish_reason: None,
        completion_tokens: None,
        upstream_provider: None,
    })
}
