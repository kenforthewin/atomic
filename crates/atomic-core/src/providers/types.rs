//! Provider type definitions

use serde::{Deserialize, Serialize};

/// Role of a message in a conversation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

/// A message in a conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: None,
        }
    }
}

/// Tool definition for function calling
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// A tool call request from the model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<ToolCallFunction>,
    // Flattened fields for simpler access
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// Get the tool name, handling both nested and flat formats
    pub fn get_name(&self) -> Option<&str> {
        self.function
            .as_ref()
            .map(|f| f.name.as_str())
            .or(self.name.as_deref())
    }

    /// Get the arguments, handling both nested and flat formats
    pub fn get_arguments(&self) -> Option<&str> {
        self.function
            .as_ref()
            .map(|f| f.arguments.as_str())
            .or(self.arguments.as_deref())
    }
}

/// Structured output schema specification
#[derive(Debug, Clone)]
pub struct StructuredOutputSchema {
    pub name: String,
    pub schema: serde_json::Value,
    pub strict: bool,
}

impl StructuredOutputSchema {
    pub fn new(name: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            schema,
            strict: true,
        }
    }
}

/// Generation parameters for LLM requests
#[derive(Debug, Clone, Default)]
pub struct GenerationParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub structured_output: Option<StructuredOutputSchema>,
    /// Optional list of parameters supported by the model (from OpenRouter API)
    /// When set, only supported parameters will be included in the request
    pub supported_parameters: Option<Vec<String>>,
    /// When true, sets reasoning effort to "minimal" for reasoning models
    /// Useful for simple tasks like tag extraction that don't need deep reasoning
    pub minimize_reasoning: bool,
}

impl GenerationParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn with_structured_output(mut self, schema: StructuredOutputSchema) -> Self {
        self.structured_output = Some(schema);
        self
    }

    pub fn with_supported_parameters(mut self, params: Vec<String>) -> Self {
        self.supported_parameters = Some(params);
        self
    }

    pub fn with_minimize_reasoning(mut self, minimize: bool) -> Self {
        self.minimize_reasoning = minimize;
        self
    }

    /// Check if a parameter is supported (returns true if no supported_parameters set)
    pub fn is_param_supported(&self, param: &str) -> bool {
        self.supported_parameters
            .as_ref()
            .map(|params| params.iter().any(|p| p == param))
            // When capabilities aren't loaded, only send universally-supported params
            .unwrap_or(matches!(
                param,
                "temperature" | "max_tokens" | "json_schema"
            ))
    }

    /// Positively-known support only: `true` iff capabilities were loaded AND
    /// include `param`. Use instead of [`Self::is_param_supported`] wherever
    /// over-sending is harmful rather than ignorable — OpenRouter's
    /// `require_parameters` routing filters to endpoints supporting EVERY sent
    /// parameter, so an optional param assumed "universal" (temperature, which
    /// the GPT-5 reasoning family rejects) empties the endpoint pool into a
    /// 404 instead of being dropped.
    pub fn is_param_known_supported(&self, param: &str) -> bool {
        self.supported_parameters
            .as_ref()
            .is_some_and(|params| params.iter().any(|p| p == param))
    }
}

/// LLM completion response
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// Streaming delta from LLM
/// Note: Some variants/fields are part of the streaming interface but not yet consumed
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum StreamDelta {
    /// Content text chunk
    Content(String),
    /// Tool call started
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// Tool call arguments (may come in chunks)
    ToolCallArguments { index: usize, arguments: String },
    /// Stream finished
    Done { finish_reason: Option<String> },
}
