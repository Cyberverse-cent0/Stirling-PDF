//! `POST /api/v1/orchestrator` — NDJSON streaming orchestrator endpoint.
//! Port of `stirling.api.routes.orchestrator`.
//!
//! Each output line is a JSON object with an `event` field: `progress`
//! events arrive as inner agents report work, the final `result` event
//! carries the typed orchestrator response, `error` events surface
//! failures without breaking the connection, and `heartbeat` events fire
//! on a fixed cadence so idle connections stay visibly alive and
//! disconnects propagate within a bounded window.

use std::convert::Infallible;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use crate::agents::orchestrator::OrchestratorAgent;
use crate::contracts::orchestrator::OrchestratorRequest;
use crate::contracts::OrchestratorResponse;
use crate::contracts::ProgressEvent;
use crate::extract::Json;
use crate::services::progress;
use crate::state::AppState;

/// Cadence for keep-alive heartbeats — Java forwards them to the frontend
/// as SSE comments; their job is to make every layer of the connection
/// visibly alive at this rhythm.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

pub fn router() -> Router<AppState> {
    Router::new().route("/", post(orchestrate))
}

enum Frame {
    Progress(ProgressEvent),
    Result(OrchestratorResponse),
    Error(String),
    Heartbeat,
    Done,
}

fn serialize_frame(frame: &Frame) -> Option<String> {
    let body = match frame {
        Frame::Progress(event) => {
            let mut v = serde_json::to_value(event).unwrap_or(Value::Null);
            v.as_object_mut()
                .map(|o| o.insert("event".to_string(), json!("progress")));
            v
        }
        Frame::Result(response) => json!({
            "event": "result",
            "response": response,
        }),
        Frame::Error(message) => json!({"event": "error", "message": message}),
        Frame::Heartbeat => json!({"event": "heartbeat"}),
        Frame::Done => return None,
    };
    Some(format!(
        "{}\n",
        serde_json::to_string(&body).unwrap_or_default()
    ))
}

async fn orchestrate(
    State(state): State<AppState>,
    Json(request): Json<OrchestratorRequest>,
) -> Response {
    if let Err(e) = request.validate() {
        return crate::error::ApiError::unprocessable(e).into_response();
    }
    let (tx, rx) = mpsc::unbounded_channel::<Frame>();

    // Progress events flow through a task-local channel into the same queue.
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressEvent>();
    let pump_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            if pump_tx.send(Frame::Progress(event)).is_err() {
                return;
            }
        }
    });

    // Heartbeat task.
    let heartbeat_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.tick().await; // first tick is immediate — skip it
        loop {
            interval.tick().await;
            if heartbeat_tx.send(Frame::Heartbeat).is_err() {
                return;
            }
        }
    });

    // Agent task.
    let agent_tx = tx.clone();
    let agent = OrchestratorAgent::new(state);
    tokio::spawn(async move {
        let result = progress::scoped(progress_tx, agent.handle(&request)).await;
        match result {
            Ok(response) => {
                let _ = agent_tx.send(Frame::Result(response));
            }
            Err(e) => {
                tracing::error!("orchestrator stream failed: {e}");
                let _ = agent_tx.send(Frame::Error(e.to_string()));
            }
        }
        let _ = agent_tx.send(Frame::Done);
    });

    let stream = UnboundedReceiverStream::new(rx)
        .filter_map(|frame| async move { serialize_frame(&frame).map(Ok::<_, Infallible>) });

    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap()
}
