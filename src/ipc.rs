//! Types exchanged between the MCP/HTTP layer (tokio) and the compositor
//! thread (calloop). Requests travel over a calloop channel; every request
//! carries a oneshot sender for its reply.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    /// Linux evdev button code, as used by wl_pointer.
    pub fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0x110,   // BTN_LEFT
            MouseButton::Right => 0x111,  // BTN_RIGHT
            MouseButton::Middle => 0x112, // BTN_MIDDLE
        }
    }
}

/// An input action to inject before observing the UI.
#[derive(Clone)]
pub enum Action {
    /// No input; just observe.
    None,
    MouseMove {
        x: f64,
        y: f64,
        window: Option<u64>,
    },
    /// press = down+up (optionally twice for double click)
    Click {
        button: MouseButton,
        x: Option<f64>,
        y: Option<f64>,
        window: Option<u64>,
        double: bool,
    },
    MouseDown {
        button: MouseButton,
        x: Option<f64>,
        y: Option<f64>,
        window: Option<u64>,
    },
    MouseUp {
        button: MouseButton,
        x: Option<f64>,
        y: Option<f64>,
        window: Option<u64>,
    },
    Scroll {
        dx: f64,
        dy: f64,
        x: Option<f64>,
        y: Option<f64>,
        window: Option<u64>,
    },
    TypeText {
        text: String,
    },
    /// e.g. "Return", "ctrl+shift+t", "F5"
    Key {
        combo: String,
        repeat: u32,
    },
    FocusWindow {
        id: u64,
    },
    CloseWindow {
        id: u64,
    },
    ResizeWindow {
        id: u64,
        width: i32,
        height: i32,
    },
}

/// How long/whether to wait for the UI to settle after the action.
#[derive(Debug, Clone)]
pub struct WaitParams {
    /// Overall convergence timeout. 0 = don't wait at all.
    pub timeout_ms: u64,
    /// Only track changes on this window (and newly created windows).
    pub watch_window: Option<u64>,
    /// Don't consider the transition complete until at least one new window
    /// appeared (used by launch_app so it doesn't return before the app maps
    /// its first window).
    pub expect_new_window: bool,
}

impl Default for WaitParams {
    fn default() -> Self {
        WaitParams {
            timeout_ms: 3000,
            watch_window: None,
            expect_new_window: false,
        }
    }
}

/// Which windows should be reported in detail (screenshot + a11y).
#[derive(Debug, Clone)]
pub enum Include {
    /// Windows that changed during the wait (plus new windows).
    Changed,
    All,
    Windows(Vec<u64>),
}

#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    pub include: Include,
    pub screenshots: bool,
}

#[derive(Debug)]
pub struct Request {
    pub action: Action,
    pub wait: WaitParams,
    pub snapshot: SnapshotRequest,
    pub reply: oneshot::Sender<Result<DesktopSnapshot, String>>,
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::None => write!(f, "None"),
            Action::MouseMove { x, y, .. } => write!(f, "MouseMove({x},{y})"),
            Action::Click { button, .. } => write!(f, "Click({button:?})"),
            Action::MouseDown { button, .. } => write!(f, "MouseDown({button:?})"),
            Action::MouseUp { button, .. } => write!(f, "MouseUp({button:?})"),
            Action::Scroll { dx, dy, .. } => write!(f, "Scroll({dx},{dy})"),
            Action::TypeText { text } => write!(f, "TypeText({} chars)", text.len()),
            Action::Key { combo, .. } => write!(f, "Key({combo})"),
            Action::FocusWindow { id } => write!(f, "FocusWindow({id})"),
            Action::CloseWindow { id } => write!(f, "CloseWindow({id})"),
            Action::ResizeWindow { id, width, height } => {
                write!(f, "ResizeWindow({id},{width}x{height})")
            }
        }
    }
}

/// Per-window state reported back to the model.
#[derive(Debug, Clone, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app_id: String,
    /// Window geometry on the virtual desktop, in pixels.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    /// Client did not answer ping requests for >10s.
    pub frozen: bool,
    /// Window committed updates during the observed transition.
    pub changed: bool,
    /// Window appeared during the observed transition.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    /// PNG-encoded screenshot; emitted separately as MCP image content.
    #[serde(skip)]
    pub screenshot_png: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DesktopSnapshot {
    pub screen_width: i32,
    pub screen_height: i32,
    pub pointer_x: f64,
    pub pointer_y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused_window: Option<u64>,
    /// True if the transition did not converge before the timeout.
    pub timed_out: bool,
    pub windows: Vec<WindowInfo>,
    /// Windows destroyed during the observed transition (their old ids).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub closed_windows: Vec<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
