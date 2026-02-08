use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::injector::{ControlCommand, InjectorSnapshot};
use crate::proofs::{ProofServingMetrics, ProofServingSnapshot};
use crate::shutdown::Shutdown;

#[derive(Clone)]
pub struct HttpServerConfig {
    pub listen_addr: SocketAddr,
    pub static_dir: PathBuf,
}

#[derive(Clone)]
struct AppState {
    injector_rx: watch::Receiver<InjectorSnapshot>,
    injector_control_tx: tokio::sync::mpsc::Sender<ControlCommand>,
    proof_metrics: Arc<ProofServingMetrics>,
    static_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct StatusResponse {
    injector: InjectorSnapshot,
    proofs: ProofServingSnapshot,
}

pub async fn run_http_server(
    shutdown: Shutdown,
    cfg: HttpServerConfig,
    injector_rx: watch::Receiver<InjectorSnapshot>,
    injector_control_tx: tokio::sync::mpsc::Sender<ControlCommand>,
    proof_metrics: Arc<ProofServingMetrics>,
) -> anyhow::Result<()> {
    let state = AppState {
        injector_rx,
        injector_control_tx,
        proof_metrics,
        static_dir: cfg.static_dir,
    };

    let api = Router::new()
        .route("/status", get(get_status))
        .route("/control/pause", post(post_pause))
        .route("/control/speed", post(post_speed))
        .route("/control/cursor", post(post_cursor));
    let assets_svc = ServeDir::new(state.static_dir.join("assets"));

    let app = Router::new()
        .nest("/api", api)
        .nest_service("/assets", assets_svc)
        .route("/", get(serve_index))
        .fallback(get(serve_index))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(cfg.listen_addr).await?;
    let server = axum::serve(listener, app);
    tokio::select! {
        res = server => res?,
        _ = shutdown.wait() => {
            // Exit promptly on Ctrl-C; dropping the server future closes the listener.
        }
    }

    Ok(())
}

async fn get_status(state: axum::extract::State<AppState>) -> impl IntoResponse {
    Json(StatusResponse {
        injector: state.injector_rx.borrow().clone(),
        proofs: state.proof_metrics.snapshot(),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct PauseRequest {
    paused: bool,
}

async fn post_pause(State(state): State<AppState>, Json(req): Json<PauseRequest>) -> impl IntoResponse {
    if state
        .injector_control_tx
        .send(ControlCommand::SetPaused { paused: req.paused })
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Clone, Deserialize)]
struct SpeedRequest {
    proofs_per_window: u32,
    window_ms: u64,
}

async fn post_speed(State(state): State<AppState>, Json(req): Json<SpeedRequest>) -> impl IntoResponse {
    if state
        .injector_control_tx
        .send(ControlCommand::SetSpeed {
            proofs_per_window: req.proofs_per_window,
            window_ms: req.window_ms,
        })
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Clone, Deserialize)]
struct CursorRequest {
    cursor_event_id: u64,
    end_event_id: Option<u64>,
}

async fn post_cursor(State(state): State<AppState>, Json(req): Json<CursorRequest>) -> impl IntoResponse {
    if let Some(end) = req.end_event_id {
        if req.cursor_event_id > end {
            return (
                StatusCode::BAD_REQUEST,
                "cursor_event_id must be <= end_event_id",
            )
                .into_response();
        }
    }

    if state
        .injector_control_tx
        .send(ControlCommand::SetCursorAndEnd {
            cursor_event_id: req.cursor_event_id,
            end_event_id: req.end_event_id,
        })
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn serve_index(state: axum::extract::State<AppState>) -> Response {
    let index_path = state.static_dir.join("index.html");
    match tokio::fs::read(index_path).await {
        Ok(contents) => {
            let mut resp = contents.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            "UI not built (missing ui/dist/index.html)",
        )
            .into_response(),
    }
}
