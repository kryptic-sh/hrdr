#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use hrdr_tools::{Tool, ToolContext};
use serde_json::{Value, json};

use crate::update_check::admit;
use crate::update_coordinator::PreparedWindowsUpdateCoordinator;

pub(crate) struct UpdateTool {
    sender: hrdr_tui::UpdateRequestSender,
}

impl UpdateTool {
    pub(crate) fn new(sender: hrdr_tui::UpdateRequestSender) -> Arc<Self> {
        Arc::new(Self { sender })
    }
}

struct PendingWindowsUpdate {
    cwd: PathBuf,
    operation_id: hrdr_agent::UpdateOperationId,
}

impl hrdr_tui::DeferredUpdate for PendingWindowsUpdate {
    fn prepare(
        self: Box<Self>,
        session_id: &str,
        recovery_cwd: &std::path::Path,
    ) -> Result<hrdr_tui::AdmittedUpdate> {
        let runtime = tokio::runtime::Handle::current();
        let checkout = runtime
            .block_on(admit(&self.cwd))
            .map_err(|failure| anyhow::anyhow!("update unavailable: {failure:?}"))?;
        Ok(hrdr_tui::AdmittedUpdate::new(
            self.operation_id.clone(),
            PreparedWindowsUpdateCoordinator::prepare(
                session_id,
                &self.operation_id,
                &checkout,
                recovery_cwd,
            )?,
        ))
    }
}

#[async_trait]
impl Tool for UpdateTool {
    fn name(&self) -> &'static str {
        "update"
    }
    fn description(&self) -> &'static str {
        "Prepare and install an approved hrdr update after the current turn finishes."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<String> {
        if args != json!({}) {
            bail!("update accepts no arguments");
        }
        let operation_id = hrdr_agent::UpdateOperationId::random();
        self.sender.send(Box::new(PendingWindowsUpdate {
            cwd: ctx.cwd.clone(),
            operation_id: operation_id.clone(),
        }))?;
        Ok(json!({
            "status": "deferred",
            "operation_id": operation_id,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schema_has_no_timeout_and_execute_rejects_arguments() {
        let (sender, _receiver) = hrdr_tui::update_request_bridge();
        let tool = UpdateTool::new(sender);
        let context = ToolContext::new(std::env::temp_dir());

        assert_eq!(
            tool.parameters(),
            json!({"type":"object","properties":{},"additionalProperties":false})
        );
        assert!(
            tool.parameters()["properties"]
                .get("timeout_secs")
                .is_none()
        );
        let error = tool
            .execute(json!({"timeout_secs": 1}), &context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("update accepts no arguments"));

        let provisional = tool.execute(json!({}), &context).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&provisional).unwrap()["status"],
            "deferred"
        );
        assert!(!provisional.contains("successful_update"));
    }
}
