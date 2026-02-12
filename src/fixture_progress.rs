use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        AnnotateAble, CallToolRequestParams, CallToolResult, GetPromptRequestParams,
        GetPromptResult, ListPromptsResult, ListResourcesResult, ListToolsResult,
        PaginatedRequestParams, ProgressNotificationParam, Prompt, PromptMessage,
        PromptMessageRole, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ServerCapabilities, ServerInfo, Tool,
    },
    service::{Peer, RequestContext},
};
use serde_json::{Map, json};
use tokio_util::sync::CancellationToken;

const TOOL_PROGRESS: &str = "fixture_progress";
const TOOL_CANCEL: &str = "fixture_cancel";
const TOOL_ECHO: &str = "fixture_echo";
const TOOL_ERROR: &str = "fixture_error";
const PROMPT_PROGRESS: &str = "fixture_progress_prompt";
const PROMPT_CANCEL: &str = "fixture_cancel_prompt";
const RESOURCE_PROGRESS_URI: &str = "fixture://progress";
const RESOURCE_CANCEL_URI: &str = "fixture://cancel";

pub async fn run() -> Result<()> {
    let service = FixtureProgressServer
        .serve(rmcp::transport::stdio())
        .await
        .context("failed to start fixture progress MCP server")?;

    let _ = service
        .waiting()
        .await
        .context("fixture progress server wait failed")?;

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct FixtureProgressServer;

impl FixtureProgressServer {
    async fn emit_progress(
        peer: &Peer<RoleServer>,
        progress_token: rmcp::model::ProgressToken,
    ) -> Result<(), McpError> {
        for step in 1..=5 {
            peer.notify_progress(ProgressNotificationParam {
                progress_token: progress_token.clone(),
                progress: step as f64,
                total: Some(5.0),
                message: Some(format!("fixture step {step}/5")),
            })
            .await
            .map_err(|err| {
                McpError::internal_error(
                    format!("failed to emit fixture progress notification: {err}"),
                    None,
                )
            })?;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        Ok(())
    }

    async fn wait_for_cancel(ct: CancellationToken) {
        let _ = tokio::time::timeout(Duration::from_secs(10), ct.cancelled()).await;
    }

    fn tool_descriptor(name: &'static str, description: &'static str) -> Tool {
        Tool::new(name, description, Arc::new(Map::new()))
    }
}

impl ServerHandler for FixtureProgressServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("fixture progress MCP server".into()),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
            ..Default::default()
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: vec![
                Self::tool_descriptor(
                    TOOL_PROGRESS,
                    "Emit deterministic progress notifications for passthrough tests",
                ),
                Self::tool_descriptor(
                    TOOL_CANCEL,
                    "Wait for cancellation to test passthrough cancellation behavior",
                ),
                Self::tool_descriptor(
                    TOOL_ECHO,
                    "Echo structured arguments for execution-bridge tests",
                ),
                Self::tool_descriptor(
                    TOOL_ERROR,
                    "Return a deterministic MCP tool error payload for execution tests",
                ),
            ],
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            TOOL_PROGRESS => {
                let progress_token = context.meta.get_progress_token().ok_or_else(|| {
                    McpError::invalid_params("progressToken is required for fixture_progress", None)
                })?;

                Self::emit_progress(&context.peer, progress_token).await?;
                Ok(CallToolResult::structured(json!({
                    "status": "fixture progress complete"
                })))
            }
            TOOL_CANCEL => {
                Self::wait_for_cancel(context.ct).await;
                Ok(CallToolResult::structured(json!({
                    "status": "fixture cancel observed"
                })))
            }
            TOOL_ECHO => Ok(CallToolResult::structured(json!({
                "echo": request.arguments.unwrap_or_default()
            }))),
            TOOL_ERROR => Ok(CallToolResult::structured_error(json!({
                "error_code": "FIXTURE_ERROR",
                "message": "fixture_error requested"
            }))),
            _ => Err(McpError::invalid_params(
                format!("unknown fixture tool '{}'", request.name),
                None,
            )),
        }
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            TOOL_PROGRESS => Some(Self::tool_descriptor(
                TOOL_PROGRESS,
                "Emit deterministic progress notifications for passthrough tests",
            )),
            TOOL_CANCEL => Some(Self::tool_descriptor(
                TOOL_CANCEL,
                "Wait for cancellation to test passthrough cancellation behavior",
            )),
            TOOL_ECHO => Some(Self::tool_descriptor(
                TOOL_ECHO,
                "Echo structured arguments for execution-bridge tests",
            )),
            TOOL_ERROR => Some(Self::tool_descriptor(
                TOOL_ERROR,
                "Return a deterministic MCP tool error payload for execution tests",
            )),
            _ => None,
        }
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult {
            meta: None,
            next_cursor: None,
            prompts: vec![
                Prompt::new(
                    PROMPT_PROGRESS,
                    Some("Prompt that emits deterministic progress notifications"),
                    None,
                ),
                Prompt::new(
                    PROMPT_CANCEL,
                    Some("Prompt that blocks until cancelled"),
                    None,
                ),
            ],
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        match request.name.as_str() {
            PROMPT_PROGRESS => {
                let progress_token = context.meta.get_progress_token().ok_or_else(|| {
                    McpError::invalid_params(
                        "progressToken is required for fixture_progress_prompt",
                        None,
                    )
                })?;

                Self::emit_progress(&context.peer, progress_token).await?;
                Ok(GetPromptResult {
                    description: Some("fixture progress prompt complete".to_string()),
                    messages: vec![PromptMessage::new_text(
                        PromptMessageRole::Assistant,
                        "fixture prompt complete",
                    )],
                })
            }
            PROMPT_CANCEL => {
                Self::wait_for_cancel(context.ct).await;
                Ok(GetPromptResult {
                    description: Some("fixture cancel prompt observed cancellation".to_string()),
                    messages: vec![PromptMessage::new_text(
                        PromptMessageRole::Assistant,
                        "fixture prompt cancel observed",
                    )],
                })
            }
            _ => Err(McpError::invalid_params(
                format!("unknown fixture prompt '{}'", request.name),
                None,
            )),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources: vec![
                rmcp::model::RawResource::new(RESOURCE_PROGRESS_URI, "fixture progress resource")
                    .no_annotation(),
                rmcp::model::RawResource::new(RESOURCE_CANCEL_URI, "fixture cancel resource")
                    .no_annotation(),
            ],
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        match request.uri.as_str() {
            RESOURCE_PROGRESS_URI => {
                let progress_token = context.meta.get_progress_token().ok_or_else(|| {
                    McpError::invalid_params(
                        "progressToken is required for fixture://progress",
                        None,
                    )
                })?;

                Self::emit_progress(&context.peer, progress_token).await?;
                Ok(ReadResourceResult {
                    contents: vec![ResourceContents::text(
                        "fixture resource progress complete",
                        RESOURCE_PROGRESS_URI,
                    )],
                })
            }
            RESOURCE_CANCEL_URI => {
                Self::wait_for_cancel(context.ct).await;
                Ok(ReadResourceResult {
                    contents: vec![ResourceContents::text(
                        "fixture resource cancel observed",
                        RESOURCE_CANCEL_URI,
                    )],
                })
            }
            _ => Err(McpError::invalid_params(
                format!("unknown fixture resource uri '{}'", request.uri),
                None,
            )),
        }
    }
}
