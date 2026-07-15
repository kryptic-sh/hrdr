use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, truncate};

use super::McpClient;

/// What an [`McpTool`] does when the model calls it.
pub(crate) enum McpOp {
    /// `tools/call` with this server-side tool name.
    Tool(String),
    ListResources,
    ReadResource,
    ListPrompts,
    GetPrompt,
}

/// One MCP capability, exposed to the model as a native [`Tool`] — either a
/// server tool or a resource/prompt list/read/get operation.
pub(crate) struct McpTool {
    pub(crate) client: Arc<McpClient>,
    pub(crate) exposed_name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) schema: Value,
    pub(crate) op: McpOp,
    pub(crate) read_only: bool,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.exposed_name
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn parameters(&self) -> Value {
        self.schema.clone()
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        let out = match &self.op {
            McpOp::Tool(name) => self.client.call_tool(name, args).await?,
            McpOp::ListResources => self.client.list_resources().await?,
            McpOp::ReadResource => {
                let uri = args
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("read_resource needs a `uri` argument"))?;
                self.client.read_resource(uri).await?
            }
            McpOp::ListPrompts => self.client.list_prompts().await?,
            McpOp::GetPrompt => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("get_prompt needs a `name` argument"))?;
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                self.client.get_prompt(name, arguments).await?
            }
        };
        // A third-party MCP server's output is external, untrusted data — wrap
        // it so injected "instructions" can't be mistaken for the harness's own.
        Ok(crate::wrap_untrusted(
            &format!("mcp tool {}", self.exposed_name),
            &truncate(&out, ctx.max_output),
        ))
    }
}
