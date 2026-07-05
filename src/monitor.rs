//! HTTP server: hosts the MCP endpoint at /mcp and a read-only, noVNC-like
//! monitoring page at / that live-streams the composited desktop for humans.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, tower::StreamableHttpServerConfig, StreamableHttpService,
};
use smithay::reexports::calloop::channel::Sender;
use tokio::sync::oneshot;

use crate::{
    a11y::A11y,
    ipc::{Action, Include, Request, SnapshotRequest, WaitParams},
    mcp::{DesktopMcp, EnvInfo},
    render::FrameStore,
};

#[derive(Clone)]
struct AppState {
    frames: Arc<FrameStore>,
    tx: Sender<Request>,
}

pub fn router(
    frames: Arc<FrameStore>,
    tx: Sender<Request>,
    a11y: Arc<Option<A11y>>,
    env: EnvInfo,
) -> Router {
    let mcp_tx = tx.clone();
    let mcp_service = StreamableHttpService::new(
        move || Ok(DesktopMcp::new(mcp_tx.clone(), a11y.clone(), env.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    Router::new()
        .route("/", get(index))
        .route("/frame.png", get(frame_png))
        .route("/state.json", get(state_json))
        .nest_service("/mcp", mcp_service)
        .with_state(AppState { frames, tx })
}

async fn frame_png(State(state): State<AppState>) -> impl IntoResponse {
    match state.frames.png() {
        Some(png) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            png,
        )
            .into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no frame rendered yet").into_response(),
    }
}

async fn state_json(State(state): State<AppState>) -> impl IntoResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    let req = Request {
        action: Action::None,
        wait: WaitParams {
            timeout_ms: 0,
            watch_window: None,
            expect_new_window: false,
        },
        snapshot: SnapshotRequest {
            include: Include::All,
            screenshots: false,
        },
        reply: reply_tx,
    };
    if state.tx.send(req).is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, "compositor gone").into_response();
    }
    match reply_rx.await {
        Ok(Ok(snapshot)) => axum::Json(snapshot).into_response(),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "snapshot failed").into_response(),
    }
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>desktop-mcp monitor</title>
<style>
  body { margin: 0; background: #16171d; color: #d8dae5; font: 13px/1.5 system-ui, sans-serif; }
  header { padding: 8px 16px; background: #20222c; display: flex; gap: 16px; align-items: baseline; }
  header h1 { font-size: 14px; margin: 0; }
  header .hint { color: #8a8fa3; }
  main { display: flex; gap: 16px; padding: 16px; flex-wrap: wrap; }
  #screen { max-width: 100%; border: 1px solid #34374a; border-radius: 4px;
            image-rendering: pixelated; background: #000; }
  aside { min-width: 260px; }
  aside h2 { font-size: 12px; text-transform: uppercase; color: #8a8fa3; margin: 0 0 6px; }
  .win { padding: 6px 8px; border: 1px solid #34374a; border-radius: 4px; margin-bottom: 6px; }
  .win .t { font-weight: 600; }
  .win .m { color: #8a8fa3; font-size: 12px; }
  .frozen { color: #ff6b6b; font-weight: 600; }
</style>
</head>
<body>
<header>
  <h1>desktop-mcp monitor</h1>
  <span class="hint">read-only view — all interaction happens through the MCP server at /mcp</span>
</header>
<main>
  <img id="screen" alt="desktop">
  <aside>
    <h2>Windows</h2>
    <div id="windows"></div>
  </aside>
</main>
<script>
const img = document.getElementById('screen');
async function refreshFrame() {
  try {
    const res = await fetch('frame.png', { cache: 'no-store' });
    if (res.ok) {
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      img.onload = () => URL.revokeObjectURL(url);
      img.src = url;
    }
  } catch (e) {}
}
async function refreshState() {
  try {
    const res = await fetch('state.json');
    if (!res.ok) return;
    const s = await res.json();
    const box = document.getElementById('windows');
    box.innerHTML = '';
    for (const w of s.windows || []) {
      const div = document.createElement('div');
      div.className = 'win';
      const frozen = w.frozen ? ' <span class="frozen">FROZEN</span>' : '';
      const focused = w.focused ? ' ●' : '';
      div.innerHTML = '<div class="t">#' + w.id + ' ' + (w.title || '(untitled)') + focused + frozen +
        '</div><div class="m">' + w.app_id + ' — ' + w.width + 'x' + w.height +
        ' @ ' + w.x + ',' + w.y + '</div>';
      box.appendChild(div);
    }
  } catch (e) {}
}
setInterval(refreshFrame, 500);
setInterval(refreshState, 1000);
refreshFrame(); refreshState();
</script>
</body>
</html>
"#;
