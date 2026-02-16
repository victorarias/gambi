use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use rmcp::RoleClient;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::worker::{WorkerConfig, WorkerContext, WorkerQuitReason, WorkerSendRequest};
use tracing::debug;
use url::Url;

const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(15);
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct LegacySseWorker {
    client: reqwest::Client,
    sse_url: String,
    auth_header: Option<String>,
}

impl LegacySseWorker {
    pub fn new(client: reqwest::Client, sse_url: String, auth_header: Option<String>) -> Self {
        Self {
            client,
            sse_url,
            auth_header,
        }
    }
}

#[derive(Debug)]
pub enum LegacySseError {
    SseConnect(String),
    SseStream(String),
    Http(String),
    Json(String),
    EndpointTimeout,
    ChannelClosed,
}

impl fmt::Display for LegacySseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SseConnect(msg) => write!(f, "SSE connection failed: {msg}"),
            Self::SseStream(msg) => write!(f, "SSE stream error: {msg}"),
            Self::Http(msg) => write!(f, "HTTP error: {msg}"),
            Self::Json(msg) => write!(f, "JSON error: {msg}"),
            Self::EndpointTimeout => write!(f, "timed out waiting for endpoint event"),
            Self::ChannelClosed => write!(f, "handler channel closed"),
        }
    }
}

impl std::error::Error for LegacySseError {}

impl rmcp::transport::worker::Worker for LegacySseWorker {
    type Error = LegacySseError;
    type Role = RoleClient;

    fn err_closed() -> Self::Error {
        LegacySseError::ChannelClosed
    }

    fn err_join(e: tokio::task::JoinError) -> Self::Error {
        LegacySseError::SseStream(format!("join error: {e}"))
    }

    fn config(&self) -> WorkerConfig {
        WorkerConfig {
            name: Some("legacy-sse".to_string()),
            channel_buffer_capacity: 16,
        }
    }

    async fn run(
        self,
        mut context: WorkerContext<Self>,
    ) -> Result<(), WorkerQuitReason<Self::Error>> {
        // Phase 1: Connect to SSE endpoint
        debug!(url = %self.sse_url, "connecting to legacy SSE endpoint");
        let mut request = self.client.get(&self.sse_url).timeout(SSE_CONNECT_TIMEOUT);
        if let Some(auth) = &self.auth_header {
            request = request.header("Authorization", format!("Bearer {auth}"));
        }

        let response = request.send().await.map_err(|e| fatal("SSE GET", e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(fatal_msg(
                "SSE GET",
                LegacySseError::SseConnect(format!("HTTP {status}")),
            ));
        }

        let byte_stream = response.bytes_stream();
        let mut sse_stream =
            sse_stream::SseStream::from_byte_stream(byte_stream.map(|r| r.map_err(SseBodyError)));

        // Phase 2: Wait for endpoint event
        debug!("waiting for endpoint event");
        let post_url = match tokio::time::timeout(
            ENDPOINT_TIMEOUT,
            discover_endpoint(&mut sse_stream),
        )
        .await
        {
            Ok(Ok(url)) => url,
            Ok(Err(e)) => return Err(fatal_msg("endpoint discovery", e)),
            Err(_) => {
                return Err(fatal_msg(
                    "endpoint discovery",
                    LegacySseError::EndpointTimeout,
                ));
            }
        };

        let post_url = resolve_post_url(&self.sse_url, &post_url)?;
        debug!(post_url = %post_url, "discovered POST endpoint");

        // Phase 3: Initialization handshake
        // Receive initialize request from handler
        let init_request = context.recv_from_handler().await?;
        self.post_message(&post_url, &init_request.message).await?;
        ack_send(init_request)?;

        // Wait for initialize response on SSE stream
        let init_response = next_json_message(&mut sse_stream).await?;
        context.send_to_handler(init_response).await?;

        // Receive initialized notification from handler
        let initialized_notif = context.recv_from_handler().await?;
        self.post_message(&post_url, &initialized_notif.message)
            .await?;
        ack_send(initialized_notif)?;

        // Phase 4: Main event loop
        debug!("entering main event loop");
        loop {
            tokio::select! {
                biased;

                _ = context.cancellation_token.cancelled() => {
                    debug!("SSE worker cancelled");
                    return Err(WorkerQuitReason::Cancelled);
                }

                sse_event = sse_stream.next() => {
                    match sse_event {
                        Some(Ok(event)) => {
                            if let Some(msg) = parse_message_event(&event)? {
                                context.send_to_handler(msg).await?;
                            }
                        }
                        Some(Err(e)) => {
                            return Err(fatal_msg(
                                "SSE stream",
                                LegacySseError::SseStream(format!("{e}")),
                            ));
                        }
                        None => {
                            debug!("SSE stream ended");
                            return Ok(());
                        }
                    }
                }

                handler_msg = context.from_handler_rx.recv() => {
                    match handler_msg {
                        Some(request) => {
                            self.post_message(&post_url, &request.message).await?;
                            ack_send(request)?;
                        }
                        None => {
                            debug!("handler channel closed");
                            return Err(WorkerQuitReason::HandlerTerminated);
                        }
                    }
                }
            }
        }
    }
}

impl LegacySseWorker {
    async fn post_message(
        &self,
        url: &str,
        message: &ClientJsonRpcMessage,
    ) -> Result<(), WorkerQuitReason<LegacySseError>> {
        let body = serde_json::to_string(message)
            .map_err(|e| fatal_msg("serialize", LegacySseError::Json(e.to_string())))?;

        let mut request = self
            .client
            .post(url)
            .timeout(POST_TIMEOUT)
            .header("Content-Type", "application/json");
        if let Some(auth) = &self.auth_header {
            request = request.header("Authorization", format!("Bearer {auth}"));
        }

        let response = request
            .body(body)
            .send()
            .await
            .map_err(|e| fatal("POST", e))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_string());
            return Err(fatal_msg(
                "POST",
                LegacySseError::Http(format!("HTTP {status}: {body}")),
            ));
        }

        Ok(())
    }
}

async fn discover_endpoint(
    stream: &mut (impl Stream<Item = Result<sse_stream::Sse, sse_stream::Error>> + Unpin),
) -> Result<String, LegacySseError> {
    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| LegacySseError::SseStream(format!("{e}")))?;
        if event.event.as_deref() == Some("endpoint") {
            if let Some(data) = event.data {
                let endpoint = data.trim().to_string();
                if endpoint.is_empty() {
                    return Err(LegacySseError::SseStream(
                        "endpoint event had empty data".to_string(),
                    ));
                }
                return Ok(endpoint);
            }
            return Err(LegacySseError::SseStream(
                "endpoint event had no data".to_string(),
            ));
        }
    }
    Err(LegacySseError::SseStream(
        "SSE stream ended before endpoint event".to_string(),
    ))
}

fn parse_message_event(
    event: &sse_stream::Sse,
) -> Result<Option<rmcp::model::ServerJsonRpcMessage>, WorkerQuitReason<LegacySseError>> {
    if event.event.as_deref() == Some("endpoint") {
        return Ok(None);
    }

    let Some(data) = &event.data else {
        return Ok(None);
    };

    let msg: rmcp::model::ServerJsonRpcMessage = serde_json::from_str(data)
        .map_err(|e| fatal_msg("parse SSE message", LegacySseError::Json(e.to_string())))?;

    Ok(Some(msg))
}

async fn next_json_message(
    stream: &mut (impl Stream<Item = Result<sse_stream::Sse, sse_stream::Error>> + Unpin),
) -> Result<rmcp::model::ServerJsonRpcMessage, WorkerQuitReason<LegacySseError>> {
    while let Some(event) = stream.next().await {
        let event = event
            .map_err(|e| fatal_msg("SSE stream", LegacySseError::SseStream(format!("{e}"))))?;

        if event.event.as_deref() == Some("endpoint") {
            continue;
        }
        let Some(data) = &event.data else {
            continue;
        };

        let msg: rmcp::model::ServerJsonRpcMessage = serde_json::from_str(data)
            .map_err(|e| fatal_msg("parse SSE message", LegacySseError::Json(e.to_string())))?;
        return Ok(msg);
    }

    Err(fatal_msg(
        "SSE stream",
        LegacySseError::SseStream("stream ended before receiving response".to_string()),
    ))
}

fn resolve_post_url(
    sse_url: &str,
    endpoint: &str,
) -> Result<String, WorkerQuitReason<LegacySseError>> {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(endpoint.to_string());
    }

    let base = Url::parse(sse_url)
        .map_err(|e| fatal_msg("resolve URL", LegacySseError::Http(e.to_string())))?;
    let resolved = base
        .join(endpoint)
        .map_err(|e| fatal_msg("resolve URL", LegacySseError::Http(e.to_string())))?;
    Ok(resolved.to_string())
}

fn ack_send(
    request: WorkerSendRequest<LegacySseWorker>,
) -> Result<(), WorkerQuitReason<LegacySseError>> {
    let _ = request.responder.send(Ok(()));
    Ok(())
}

fn fatal(context: &str, e: reqwest::Error) -> WorkerQuitReason<LegacySseError> {
    WorkerQuitReason::Fatal {
        error: LegacySseError::Http(e.to_string()),
        context: Cow::Owned(context.to_string()),
    }
}

fn fatal_msg(context: &str, error: LegacySseError) -> WorkerQuitReason<LegacySseError> {
    WorkerQuitReason::Fatal {
        error,
        context: Cow::Owned(context.to_string()),
    }
}

#[derive(Debug)]
struct SseBodyError(reqwest::Error);

impl fmt::Display for SseBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SseBodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}
