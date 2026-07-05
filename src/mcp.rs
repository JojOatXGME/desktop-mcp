//! The MCP server: every tool injects input (or nothing) into the compositor
//! and returns the settled UI state — window list, screenshots of changed
//! windows and accessibility metadata — so the model never has to poll.

use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use smithay::reexports::calloop::channel::Sender;
use tokio::sync::oneshot;

use crate::{
    a11y::A11y,
    ipc::{Action, DesktopSnapshot, Include, MouseButton, Request, SnapshotRequest, WaitParams},
};

#[derive(Clone)]
pub struct EnvInfo {
    pub wayland_display: String,
    #[allow(dead_code)]
    pub mcp_url: String,
    pub monitor_url: String,
}

#[derive(Clone)]
pub struct DesktopMcp {
    tx: Sender<Request>,
    a11y: Arc<Option<A11y>>,
    env: EnvInfo,
    tool_router: ToolRouter<Self>,
}

// ---------------------------------------------------------------------
// Parameter types
// ---------------------------------------------------------------------

#[derive(Deserialize, JsonSchema, Default)]
pub struct ObserveParams {
    /// Max milliseconds to wait for the UI to settle before returning
    /// (default 3000). 0 returns immediately without waiting.
    #[serde(default)]
    pub wait_ms: Option<u64>,
    /// Only track changes of this window id (and of newly created windows).
    #[serde(default)]
    pub watch_window: Option<u64>,
    /// Attach screenshots of the reported windows (default true).
    #[serde(default)]
    pub screenshots: Option<bool>,
    /// Attach accessibility (AT-SPI) metadata of the reported windows
    /// (default true).
    #[serde(default)]
    pub accessibility: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetStateParams {
    /// Restrict the detailed report (screenshot + accessibility) to these
    /// window ids. Omit to report all windows.
    #[serde(default)]
    pub windows: Option<Vec<u64>>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Middle,
    Right,
}

impl From<Button> for MouseButton {
    fn from(b: Button) -> Self {
        match b {
            Button::Left => MouseButton::Left,
            Button::Middle => MouseButton::Middle,
            Button::Right => MouseButton::Right,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ClickParams {
    /// X coordinate. Window-relative if `window` is set, otherwise global
    /// desktop coordinates. Omit x/y to click at the current pointer
    /// position (or the window center if `window` is set).
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    /// Window id; makes x/y relative to that window's top-left corner.
    #[serde(default)]
    pub window: Option<u64>,
    #[serde(default)]
    pub button: Option<Button>,
    /// Double-click instead of a single click.
    #[serde(default)]
    pub double: Option<bool>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct MouseButtonParams {
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    /// Window id; makes x/y relative to that window's top-left corner.
    #[serde(default)]
    pub window: Option<u64>,
    #[serde(default)]
    pub button: Option<Button>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct MouseMoveParams {
    pub x: f64,
    pub y: f64,
    /// Window id; makes x/y relative to that window's top-left corner.
    #[serde(default)]
    pub window: Option<u64>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct ScrollParams {
    /// Vertical scroll steps; positive scrolls down.
    #[serde(default)]
    pub dy: Option<f64>,
    /// Horizontal scroll steps; positive scrolls right.
    #[serde(default)]
    pub dx: Option<f64>,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub window: Option<u64>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct TypeTextParams {
    /// Text to type into the focused window. '\n' presses Return, '\t' Tab.
    pub text: String,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct PressKeyParams {
    /// Key or combo: a character ("a"), a key name ("Return", "Escape",
    /// "F5", "Down") or modifiers+key ("ctrl+c", "ctrl+shift+t", "alt+F4").
    pub key: String,
    /// Press the key this many times (default 1).
    #[serde(default)]
    pub repeat: Option<u32>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct WindowParams {
    pub window: u64,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct ResizeParams {
    pub window: u64,
    pub width: i32,
    pub height: i32,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

#[derive(Deserialize, JsonSchema)]
pub struct LaunchParams {
    /// Program to launch. Run with `sh -c` when `args` is omitted, so shell
    /// syntax is allowed.
    pub command: String,
    /// Explicit argument vector; when set, `command` is executed directly.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(flatten)]
    pub observe: ObserveParams,
}

// ---------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------

impl DesktopMcp {
    pub fn new(tx: Sender<Request>, a11y: Arc<Option<A11y>>, env: EnvInfo) -> Self {
        DesktopMcp {
            tx,
            a11y,
            env,
            tool_router: Self::tool_router(),
        }
    }

    async fn perform_launch(
        &self,
        observe: &ObserveParams,
    ) -> Result<CallToolResult, ErrorData> {
        self.perform(Action::None, observe, Include::Changed, 8000, true)
            .await
    }

    async fn perform(
        &self,
        action: Action,
        observe: &ObserveParams,
        include: Include,
        default_wait_ms: u64,
        expect_new_window: bool,
    ) -> Result<CallToolResult, ErrorData> {
        let wait_ms = observe.wait_ms.unwrap_or(default_wait_ms);
        let screenshots = observe.screenshots.unwrap_or(true);
        let accessibility = observe.accessibility.unwrap_or(true);

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = Request {
            action,
            wait: WaitParams {
                timeout_ms: wait_ms,
                watch_window: observe.watch_window,
                expect_new_window,
            },
            snapshot: SnapshotRequest {
                include,
                screenshots,
            },
            reply: reply_tx,
        };
        self.tx
            .send(request)
            .map_err(|_| ErrorData::internal_error("compositor is gone", None))?;

        let snapshot = tokio::time::timeout(
            Duration::from_millis(wait_ms + 30_000),
            reply_rx,
        )
        .await
        .map_err(|_| ErrorData::internal_error("timed out waiting for the compositor", None))?
        .map_err(|_| ErrorData::internal_error("compositor dropped the request", None))?
        .map_err(|e| ErrorData::invalid_params(e, None))?;

        self.render_result(snapshot, accessibility).await
    }

    async fn render_result(
        &self,
        snapshot: DesktopSnapshot,
        accessibility: bool,
    ) -> Result<CallToolResult, ErrorData> {
        let mut state = serde_json::to_value(&snapshot)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

        // Attach accessibility trees to the windows that carry a screenshot
        // (i.e. the ones reported in detail).
        if accessibility {
            if let Some(a11y) = self.a11y.as_ref() {
                let mut trees = a11y.snapshot().await;
                if let Some(windows) = state
                    .get_mut("windows")
                    .and_then(Value::as_array_mut)
                {
                    for (info, win_json) in snapshot.windows.iter().zip(windows.iter_mut()) {
                        if info.screenshot_png.is_none() {
                            continue;
                        }
                        let Some(pid) = info.pid else { continue };
                        let Some(app_trees) = trees.get_mut(&pid) else {
                            continue;
                        };
                        // Prefer the frame whose accessible name matches the
                        // window title; fall back to all frames of the app.
                        let matching: Vec<Value> = app_trees
                            .iter()
                            .filter(|t| {
                                t.get("name").and_then(Value::as_str) == Some(&info.title)
                            })
                            .cloned()
                            .collect();
                        let value = if matching.len() == 1 {
                            matching.into_iter().next().unwrap()
                        } else if app_trees.len() == 1 {
                            app_trees[0].clone()
                        } else if !matching.is_empty() {
                            json!(matching)
                        } else if !app_trees.is_empty() {
                            json!(app_trees)
                        } else {
                            continue;
                        };
                        win_json
                            .as_object_mut()
                            .unwrap()
                            .insert("accessibility".into(), value);
                    }
                }
            }
        }

        let mut contents = vec![ContentBlock::text(
            serde_json::to_string_pretty(&state)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
        )];
        for info in &snapshot.windows {
            if let Some(png) = &info.screenshot_png {
                use base64::Engine as _;
                contents.push(ContentBlock::text(format!(

                    "Screenshot of window {} ({:?}), {}x{} px. Coordinates inside this \
                     image are window-relative coordinates for window {}.",
                    info.id, info.title, info.width, info.height, info.id
                )));
                contents.push(ContentBlock::image(
                    base64::engine::general_purpose::STANDARD.encode(png),
                    "image/png",
                ));
            }
        }
        Ok(CallToolResult::success(contents))
    }
}

#[tool_router]
impl DesktopMcp {
    #[tool(
        name = "get_state",
        description = "Observe the desktop: waits until the UI settles, then returns the list of \
                       open windows plus a screenshot and accessibility metadata for each \
                       requested window. Use wait_ms=0 for an immediate snapshot."
    )]
    async fn get_state(
        &self,
        params: Parameters<GetStateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let include = match p.windows {
            Some(ids) => Include::Windows(ids),
            None => Include::All,
        };
        self.perform(Action::None, &p.observe, include, 1500, false).await
    }

    #[tool(
        name = "click",
        description = "Click a mouse button (default left, optional double-click) at the given \
                       position, then wait for the UI to settle and return the state of every \
                       window that changed (screenshot + accessibility metadata). Coordinates \
                       are window-relative when `window` is set, otherwise global."
    )]
    async fn click(&self, params: Parameters<ClickParams>) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let action = Action::Click {
            button: p.button.unwrap_or_default().into(),
            x: p.x,
            y: p.y,
            window: p.window,
            double: p.double.unwrap_or(false),
        };
        self.perform(action, &p.observe, Include::Changed, 3000, false).await
    }

    #[tool(
        name = "mouse_down",
        description = "Press and hold a mouse button (for drag & drop: mouse_down, mouse_move, \
                       mouse_up). Returns the settled UI state afterwards."
    )]
    async fn mouse_down(
        &self,
        params: Parameters<MouseButtonParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let action = Action::MouseDown {
            button: p.button.unwrap_or_default().into(),
            x: p.x,
            y: p.y,
            window: p.window,
        };
        self.perform(action, &p.observe, Include::Changed, 3000, false).await
    }

    #[tool(
        name = "mouse_up",
        description = "Release a previously pressed mouse button. Returns the settled UI state."
    )]
    async fn mouse_up(
        &self,
        params: Parameters<MouseButtonParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let action = Action::MouseUp {
            button: p.button.unwrap_or_default().into(),
            x: p.x,
            y: p.y,
            window: p.window,
        };
        self.perform(action, &p.observe, Include::Changed, 3000, false).await
    }

    #[tool(
        name = "mouse_move",
        description = "Move the pointer (hover). Coordinates are window-relative when `window` \
                       is set. Returns the settled UI state."
    )]
    async fn mouse_move(
        &self,
        params: Parameters<MouseMoveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let action = Action::MouseMove {
            x: p.x,
            y: p.y,
            window: p.window,
        };
        self.perform(action, &p.observe, Include::Changed, 3000, false).await
    }

    #[tool(
        name = "scroll",
        description = "Scroll with the mouse wheel at the given (or current) pointer position. \
                       dy>0 scrolls down, dy<0 up; dx for horizontal. One unit is one wheel \
                       detent. Returns the settled UI state."
    )]
    async fn scroll(&self, params: Parameters<ScrollParams>) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let action = Action::Scroll {
            dx: p.dx.unwrap_or(0.0),
            dy: p.dy.unwrap_or(0.0),
            x: p.x,
            y: p.y,
            window: p.window,
        };
        self.perform(action, &p.observe, Include::Changed, 3000, false).await
    }

    #[tool(
        name = "type_text",
        description = "Type text on the virtual keyboard into the focused window. '\\n' presses \
                       Return, '\\t' presses Tab. Returns the settled UI state."
    )]
    async fn type_text(
        &self,
        params: Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        self.perform(
            Action::TypeText { text: p.text },
            &p.observe,
            Include::Changed,
            3000,
            false,
        )
        .await
    }

    #[tool(
        name = "press_key",
        description = "Press a key or key combination, e.g. 'Return', 'Escape', 'Tab', 'Down', \
                       'F5', 'ctrl+c', 'ctrl+shift+t', 'alt+F4'. Returns the settled UI state."
    )]
    async fn press_key(
        &self,
        params: Parameters<PressKeyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let action = Action::Key {
            combo: p.key,
            repeat: p.repeat.unwrap_or(1),
        };
        self.perform(action, &p.observe, Include::Changed, 3000, false).await
    }

    #[tool(
        name = "focus_window",
        description = "Give keyboard focus to a window and raise it. Returns the settled UI state."
    )]
    async fn focus_window(
        &self,
        params: Parameters<WindowParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        self.perform(
            Action::FocusWindow { id: p.window },
            &p.observe,
            Include::Changed,
            3000,
            false,
        )
        .await
    }

    #[tool(
        name = "close_window",
        description = "Ask a window to close (like clicking its close button). The app may show \
                       a confirmation dialog instead of closing. Returns the settled UI state."
    )]
    async fn close_window(
        &self,
        params: Parameters<WindowParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        self.perform(
            Action::CloseWindow { id: p.window },
            &p.observe,
            Include::Changed,
            3000,
            false,
        )
        .await
    }

    #[tool(
        name = "resize_window",
        description = "Ask a window to resize to the given size in pixels. Returns the settled \
                       UI state."
    )]
    async fn resize_window(
        &self,
        params: Parameters<ResizeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        self.perform(
            Action::ResizeWindow {
                id: p.window,
                width: p.width,
                height: p.height,
            },
            &p.observe,
            Include::Changed,
            3000,
            false,
        )
        .await
    }

    #[tool(
        name = "launch_app",
        description = "Launch an application on this Wayland desktop (WAYLAND_DISPLAY, D-Bus \
                       and accessibility environment are prepared). Waits for the app's window \
                       to appear and returns the settled UI state."
    )]
    async fn launch_app(
        &self,
        params: Parameters<LaunchParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        let mut cmd = match &p.args {
            Some(args) => {
                let mut c = std::process::Command::new(&p.command);
                c.args(args);
                c
            }
            None => {
                let mut c = std::process::Command::new("sh");
                c.arg("-c").arg(&p.command);
                c
            }
        };
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.spawn()
            .map_err(|e| ErrorData::invalid_params(format!("failed to launch: {e}"), None))?;

        // Wait generously: application startup is slower than a UI transition.
        let mut observe = p.observe;
        if observe.wait_ms.is_none() {
            observe.wait_ms = Some(8000);
        }
        self.perform_launch(&observe)
            .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DesktopMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(format!(
                "Virtual Wayland desktop ({display}). Every tool injects input and then waits \
                 until the UI settles (two quiet ping rounds or wait_ms timeout), returning the \
                 window list plus screenshots and accessibility metadata of changed windows — \
                 no separate screenshot polling is needed. Coordinates: pass `window` to use \
                 window-relative coordinates matching that window's screenshot; without it, \
                 coordinates are global desktop pixels. Start apps with launch_app. A human can \
                 watch the desktop read-only at {monitor}.",
                display = self.env.wayland_display,
                monitor = self.env.monitor_url,
        ));
        info
    }
}
