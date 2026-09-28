use anyhow::Result;
use tokio::sync::mpsc;

pub type Token = String;

pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
}

#[async_trait::async_trait]
pub trait LlmBackend: Send + Sync {
    /// `json_schema`, if given, constrains output to that JSON schema (passed as
    /// LM Studio `response_format.json_schema`); a backend that doesn't support
    /// structured output silently ignores it.
    async fn generate(
        &self,
        messages: &[Message],
        max_tokens: usize,
        tx: mpsc::Sender<Token>,
        json_schema: Option<serde_json::Value>,
    ) -> Result<()>;
}
