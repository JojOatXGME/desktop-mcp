//! The headless Wayland compositor: smithay state, protocol handlers, input
//! injection and snapshot building. Runs on the main thread inside a calloop
//! event loop; the MCP layer talks to it through a calloop channel.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use smithay::{
    backend::renderer::{
        damage::OutputDamageTracker, pixman::PixmanRenderer, utils::on_commit_buffer_handler,
    },
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{find_popup_root_surface, PopupKind, PopupManager, Space, Window},
    input::{
        keyboard::{FilterResult, Keycode, XkbConfig},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
        Seat, SeatHandler, SeatState,
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::LoopHandle,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
            Client, DisplayHandle, Resource,
        },
    },
    utils::{Logical, Point, Rectangle, Serial, Size, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, with_states, CompositorClientState, CompositorHandler,
            CompositorState,
        },
        output::{OutputHandler, OutputManagerState},
        selection::{
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
            SelectionHandler,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ShellClient, ToplevelSurface, XdgPopupSurfaceData,
            XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::{
    ipc::{Action, DesktopSnapshot, Include, MouseButton, Request, WindowInfo},
    keys::KeyMapper,
    render::FrameStore,
    tracker::{PingState, Wait},
};

pub struct DesktopState {
    pub display_handle: DisplayHandle,
    #[allow(dead_code)]
    pub loop_handle: LoopHandle<'static, DesktopState>,
    pub start_time: Instant,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    #[allow(dead_code)] // keeps the xdg-output global alive
    pub output_manager_state: OutputManagerState,

    pub space: Space<Window>,
    pub popups: PopupManager,
    pub output: Output,
    pub seat: Seat<Self>,

    pub renderer: PixmanRenderer,
    pub damage_tracker: OutputDamageTracker,
    pub frames: Arc<FrameStore>,

    pub keymapper: KeyMapper,
    pub pointer_loc: Point<f64, Logical>,
    pub screen_size: Size<i32, Logical>,

    pub windows: HashMap<u64, Window>,
    pub next_window_id: u64,
    pub focused_window: Option<u64>,

    pub ping_states: HashMap<u64, PingState>,
    pub waits: Vec<Wait>,
}

/// Tag stored in each Window's user data to identify it across the API.
pub struct WindowIdTag(pub u64);

/// Tag stored in each xdg ShellClient's user data for ping bookkeeping.
struct ShellClientKey(u64);

pub fn shell_client_key(sc: &ShellClient) -> Option<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    sc.with_data(|map| {
        map.insert_if_missing(|| ShellClientKey(NEXT.fetch_add(1, Ordering::Relaxed)));
        map.get::<ShellClientKey>().unwrap().0
    })
    .ok()
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl DesktopState {
    pub fn new(
        display_handle: DisplayHandle,
        loop_handle: LoopHandle<'static, DesktopState>,
        size: (i32, i32),
        frames: Arc<FrameStore>,
    ) -> anyhow::Result<Self> {
        let dh = &display_handle;
        let compositor_state = CompositorState::new::<Self>(dh);
        let xdg_shell_state = XdgShellState::new::<Self>(dh);
        let shm_state = ShmState::new::<Self>(dh, vec![]);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<Self>(dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(dh);

        let mut seat: Seat<Self> = seat_state.new_wl_seat(dh, "desktop-mcp");
        // Pin the layout explicitly: XkbConfig::default() would fall back to
        // the host's XKB_DEFAULT_* environment, but input injection (KeyMapper)
        // resolves characters against the us layout.
        seat.add_keyboard(
            XkbConfig {
                layout: "us",
                ..XkbConfig::default()
            },
            400,
            40,
        )
        .map_err(|e| anyhow::anyhow!("failed to add keyboard: {e:?}"))?;
        seat.add_pointer();

        let output = Output::new(
            "desktop-mcp-0".to_string(),
            PhysicalProperties {
                size: (size.0 * 25 / 96, size.1 * 25 / 96).into(),
                subpixel: Subpixel::Unknown,
                make: "desktop-mcp".to_string(),
                model: "virtual".to_string(),
            },
        );
        let _global = output.create_global::<Self>(dh);
        let mode = OutputMode {
            size: size.into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(smithay::utils::Transform::Normal),
            Some(smithay::output::Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        let renderer = PixmanRenderer::new()?;
        let damage_tracker = OutputDamageTracker::from_output(&output);

        Ok(DesktopState {
            display_handle,
            loop_handle,
            start_time: Instant::now(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            data_device_state,
            output_manager_state,
            space,
            popups: PopupManager::default(),
            output,
            seat,
            renderer,
            damage_tracker,
            frames,
            keymapper: KeyMapper::new()?,
            pointer_loc: (size.0 as f64 / 2.0, size.1 as f64 / 2.0).into(),
            screen_size: size.into(),
            windows: HashMap::new(),
            next_window_id: 1,
            focused_window: None,
            ping_states: HashMap::new(),
            waits: Vec::new(),
        })
    }

    pub fn now_ms(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    pub fn window_id(window: &Window) -> Option<u64> {
        window.user_data().get::<WindowIdTag>().map(|t| t.0)
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// Map any committed surface (toplevel, subsurface or popup) to the id of
    /// the window it belongs to.
    fn window_id_for_commit(&self, surface: &WlSurface) -> Option<u64> {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(window) = self.window_for_surface(&root) {
            return Self::window_id(&window);
        }
        // popups: resolve to their toplevel ancestor
        if let Some(popup) = self.popups.find_popup(&root) {
            if let Ok(top) = find_popup_root_surface(&popup) {
                if let Some(window) = self.window_for_surface(&top) {
                    return Self::window_id(&window);
                }
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // Request handling (called from the calloop channel source)
    // ------------------------------------------------------------------

    pub fn handle_request(&mut self, req: Request) {
        let Request {
            action,
            wait,
            snapshot,
            reply,
        } = req;
        tracing::debug!(?action, "handling request");
        let warnings = match self.perform_action(&action) {
            Ok(w) => w,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        let _ = self.display_handle.flush_clients();

        if wait.timeout_ms == 0 {
            let snap = self.build_snapshot(
                &snapshot.include,
                snapshot.screenshots,
                &HashSet::new(),
                None,
                &[],
                false,
                warnings,
            );
            let _ = reply.send(Ok(snap));
        } else {
            self.start_wait(wait, snapshot, reply, warnings);
        }
    }

    fn perform_action(&mut self, action: &Action) -> Result<Vec<String>, String> {
        let mut warnings = Vec::new();
        match action.clone() {
            Action::None => {}
            Action::MouseMove { x, y, window } => {
                let pos = self.resolve_point(Some(x), Some(y), window)?;
                self.pointer_motion(pos);
            }
            Action::Click {
                button,
                x,
                y,
                window,
                double,
            } => {
                if let Some(pos) = self.optional_point(x, y, window)? {
                    self.pointer_motion(pos);
                }
                self.focus_under_pointer();
                let n = if double { 2 } else { 1 };
                for _ in 0..n {
                    self.pointer_button(button, true);
                    self.pointer_button(button, false);
                }
            }
            Action::MouseDown {
                button,
                x,
                y,
                window,
            } => {
                if let Some(pos) = self.optional_point(x, y, window)? {
                    self.pointer_motion(pos);
                }
                self.focus_under_pointer();
                self.pointer_button(button, true);
            }
            Action::MouseUp {
                button,
                x,
                y,
                window,
            } => {
                if let Some(pos) = self.optional_point(x, y, window)? {
                    self.pointer_motion(pos);
                }
                self.pointer_button(button, false);
            }
            Action::Scroll {
                dx,
                dy,
                x,
                y,
                window,
            } => {
                if let Some(pos) = self.optional_point(x, y, window)? {
                    self.pointer_motion(pos);
                }
                let time = self.now_ms();
                let pointer = self.seat.get_pointer().unwrap();
                let mut frame = AxisFrame::new(time)
                    .source(smithay::backend::input::AxisSource::Wheel);
                if dy != 0.0 {
                    frame = frame
                        .value(smithay::backend::input::Axis::Vertical, dy * 15.0)
                        .v120(smithay::backend::input::Axis::Vertical, (dy * 120.0) as i32);
                }
                if dx != 0.0 {
                    frame = frame
                        .value(smithay::backend::input::Axis::Horizontal, dx * 15.0)
                        .v120(
                            smithay::backend::input::Axis::Horizontal,
                            (dx * 120.0) as i32,
                        );
                }
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            Action::TypeText { text } => {
                self.ensure_keyboard_focus();
                for c in text.chars() {
                    match self.keymapper.for_char(c) {
                        Some(press) => self.tap_key(press.keycode, press.shift),
                        None => warnings.push(format!(
                            "character {c:?} cannot be typed with the us keymap; skipped"
                        )),
                    }
                }
            }
            Action::Key { combo, repeat } => {
                self.ensure_keyboard_focus();
                let (mods, key) = self.keymapper.parse_combo(&combo)?;
                for m in &mods {
                    self.key_event(*m, true);
                }
                for _ in 0..repeat.max(1) {
                    self.tap_key(key.keycode, key.shift && !combo.to_lowercase().contains("shift"));
                }
                for m in mods.iter().rev() {
                    self.key_event(*m, false);
                }
            }
            Action::FocusWindow { id } => self.focus_window_by_id(id)?,
            Action::CloseWindow { id } => {
                let window = self.window_by_id(id)?;
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
            Action::ResizeWindow { id, width, height } => {
                let window = self.window_by_id(id)?;
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.size = Some((width.max(1), height.max(1)).into());
                    });
                    toplevel.send_configure();
                }
            }
        }
        Ok(warnings)
    }

    fn window_by_id(&self, id: u64) -> Result<Window, String> {
        self.windows
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no window with id {id}"))
    }

    fn resolve_point(
        &self,
        x: Option<f64>,
        y: Option<f64>,
        window: Option<u64>,
    ) -> Result<Point<f64, Logical>, String> {
        let (x, y) = match (x, y) {
            (Some(x), Some(y)) => (x, y),
            _ => return Ok(self.pointer_loc),
        };
        match window {
            Some(id) => {
                let window = self.window_by_id(id)?;
                let geo = self
                    .space
                    .element_geometry(&window)
                    .ok_or_else(|| format!("window {id} is not mapped"))?;
                Ok((geo.loc.x as f64 + x, geo.loc.y as f64 + y).into())
            }
            None => Ok((x, y).into()),
        }
    }

    fn optional_point(
        &self,
        x: Option<f64>,
        y: Option<f64>,
        window: Option<u64>,
    ) -> Result<Option<Point<f64, Logical>>, String> {
        if x.is_none() && y.is_none() && window.is_none() {
            return Ok(None);
        }
        // window given without coordinates: aim at the window center
        if x.is_none() && y.is_none() {
            if let Some(id) = window {
                let w = self.window_by_id(id)?;
                let geo = self
                    .space
                    .element_geometry(&w)
                    .ok_or_else(|| format!("window {id} is not mapped"))?;
                return Ok(Some(
                    (
                        geo.loc.x as f64 + geo.size.w as f64 / 2.0,
                        geo.loc.y as f64 + geo.size.h as f64 / 2.0,
                    )
                        .into(),
                ));
            }
        }
        self.resolve_point(x, y, window).map(Some)
    }

    // ------------------------------------------------------------------
    // Input injection primitives
    // ------------------------------------------------------------------

    fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, loc)| {
            window
                .surface_under(pos - loc.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
                .map(|(surface, surface_loc)| (surface, (surface_loc + loc).to_f64()))
        })
    }

    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>) {
        let pos: Point<f64, Logical> = (
            pos.x.clamp(0.0, (self.screen_size.w - 1) as f64),
            pos.y.clamp(0.0, (self.screen_size.h - 1) as f64),
        )
            .into();
        self.pointer_loc = pos;
        let under = self.surface_under(pos);
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.now_ms();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    fn pointer_button(&mut self, button: MouseButton, pressed: bool) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.now_ms();
        let pointer = self.seat.get_pointer().unwrap();
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time,
                button: button.code(),
                state: if pressed {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        pointer.frame(self);
    }

    /// Click-to-focus: focus and raise the window under the pointer.
    fn focus_under_pointer(&mut self) {
        if let Some((window, _)) = self
            .space
            .element_under(self.pointer_loc)
            .map(|(w, l)| (w.clone(), l))
        {
            if let Some(id) = Self::window_id(&window) {
                let _ = self.focus_window_by_id(id);
            }
        }
    }

    fn ensure_keyboard_focus(&mut self) {
        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.current_focus().is_some() {
            return;
        }
        let top = self
            .space
            .elements()
            .last()
            .and_then(Self::window_id);
        if let Some(id) = top {
            let _ = self.focus_window_by_id(id);
        }
    }

    fn key_event(&mut self, keycode: Keycode, pressed: bool) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.now_ms();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.input::<(), _>(
            self,
            keycode,
            if pressed {
                smithay::backend::input::KeyState::Pressed
            } else {
                smithay::backend::input::KeyState::Released
            },
            serial,
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    fn tap_key(&mut self, keycode: Keycode, shift: bool) {
        if shift {
            self.key_event(KeyMapper::shift_keycode(), true);
        }
        self.key_event(keycode, true);
        self.key_event(keycode, false);
        if shift {
            self.key_event(KeyMapper::shift_keycode(), false);
        }
    }

    pub fn focus_window_by_id(&mut self, id: u64) -> Result<(), String> {
        let window = self.window_by_id(id)?;
        self.space.raise_element(&window, true);
        let all: Vec<Window> = self.space.elements().cloned().collect();
        for w in all {
            let active = Self::window_id(&w) == Some(id);
            if let Some(toplevel) = w.toplevel() {
                toplevel.with_pending_state(|state| {
                    if active {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                toplevel.send_pending_configure();
            }
        }
        let surface = window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .ok_or_else(|| format!("window {id} has no toplevel surface"))?;
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, Some(surface), serial);
        self.focused_window = Some(id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Snapshot building
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn build_snapshot(
        &mut self,
        include: &Include,
        screenshots: bool,
        changed: &HashSet<u64>,
        known_at_start: Option<&HashSet<u64>>,
        closed: &[u64],
        timed_out: bool,
        warnings: Vec<String>,
    ) -> DesktopSnapshot {
        let windows_top_down: Vec<Window> = self.space.elements().rev().cloned().collect();
        let mut infos = Vec::new();
        for window in windows_top_down {
            let Some(id) = Self::window_id(&window) else {
                continue;
            };
            let Some(toplevel) = window.toplevel().cloned() else {
                continue;
            };
            let geo = self
                .space
                .element_geometry(&window)
                .unwrap_or_else(|| Rectangle::from_size((0, 0).into()));
            let (title, app_id) = with_states(toplevel.wl_surface(), |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .map(|d| {
                        let d = d.lock().unwrap();
                        (
                            d.title.clone().unwrap_or_default(),
                            d.app_id.clone().unwrap_or_default(),
                        )
                    })
                    .unwrap_or_default()
            });
            let pid = toplevel
                .wl_surface()
                .client()
                .and_then(|c| c.get_credentials(&self.display_handle).ok())
                .map(|c| c.pid);
            let frozen = toplevel
                .client()
                .pipe(|sc| shell_client_key(&sc))
                .map(|key| self.client_frozen(key))
                .unwrap_or(false);
            let is_new = known_at_start
                .map(|known| !known.contains(&id))
                .unwrap_or(false);
            let detail = match include {
                Include::All => true,
                Include::Changed => changed.contains(&id) || is_new,
                Include::Windows(ids) => ids.contains(&id),
            };
            let screenshot_png = if detail && screenshots {
                crate::render::capture_window(&mut self.renderer, &window, geo.size)
            } else {
                None
            };
            infos.push(WindowInfo {
                id,
                title,
                app_id,
                x: geo.loc.x,
                y: geo.loc.y,
                width: geo.size.w,
                height: geo.size.h,
                focused: self.focused_window == Some(id),
                frozen,
                changed: changed.contains(&id) || is_new,
                new: is_new,
                pid,
                screenshot_png,
            });
        }
        DesktopSnapshot {
            screen_width: self.screen_size.w,
            screen_height: self.screen_size.h,
            pointer_x: self.pointer_loc.x,
            pointer_y: self.pointer_loc.y,
            focused_window: self.focused_window,
            timed_out,
            windows: infos,
            closed_windows: closed.to_vec(),
            warnings,
        }
    }
}

/// Tiny helper: method-chaining for plain values.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T: Sized> Pipe for T {}

// ----------------------------------------------------------------------
// smithay protocol handlers
// ----------------------------------------------------------------------

impl CompositorHandler for DesktopState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.window_for_surface(&root) {
                window.on_commit();
            }
        }
        self.popups.commit(surface);
        ensure_initial_configure(self, surface);
        if let Some(id) = self.window_id_for_commit(surface) {
            self.note_window_activity(id);
        }
    }
}

fn ensure_initial_configure(state: &mut DesktopState, surface: &WlSurface) {
    if let Some(window) = state.window_for_surface(surface) {
        if let Some(toplevel) = window.toplevel() {
            let initial_configure_sent = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .map(|d| d.lock().unwrap().initial_configure_sent)
                    .unwrap_or(true)
            });
            if !initial_configure_sent {
                toplevel.send_configure();
            }
        }
        return;
    }
    if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface) {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgPopupSurfaceData>()
                .map(|d| d.lock().unwrap().initial_configure_sent)
                .unwrap_or(true)
        });
        if !initial_configure_sent {
            let _ = popup.send_configure();
        }
    }
}

impl BufferHandler for DesktopState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for DesktopState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for DesktopState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_window_id;
        self.next_window_id += 1;

        let window = Window::new_wayland_window(surface);
        window.user_data().insert_if_missing(|| WindowIdTag(id));

        // Cascade new windows so they don't fully overlap.
        let n = self.windows.len() as i32;
        let pos = (24 + 32 * (n % 16), 24 + 32 * (n % 16));
        self.space.map_element(window.clone(), pos, true);
        self.windows.insert(id, window);
        self.note_window_activity(id);
        let _ = self.focus_window_by_id(id);
        tracing::info!(window = id, "new toplevel");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self.window_for_surface(surface.wl_surface());
        if let Some(window) = window {
            if let Some(id) = Self::window_id(&window) {
                self.space.unmap_elem(&window);
                self.windows.remove(&id);
                self.note_window_closed(id);
                if self.focused_window == Some(id) {
                    self.focused_window = None;
                    let top = self.space.elements().last().and_then(Self::window_id);
                    if let Some(top) = top {
                        let _ = self.focus_window_by_id(top);
                    }
                }
                tracing::info!(window = id, "toplevel destroyed");
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        // report activity on the parent window so the change is observed
        if let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(surface)) {
            if let Some(window) = self.window_for_surface(&root) {
                if let Some(id) = Self::window_id(&window) {
                    self.note_window_activity(id);
                }
            }
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Popup grabs are not needed for injected input; popups still render
        // and receive pointer events through regular focus resolution.
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn client_pong(&mut self, client: ShellClient) {
        if let Some(key) = shell_client_key(&client) {
            self.note_pong(key);
        }
    }
}

impl SeatHandler for DesktopState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        self.focused_window = focused
            .and_then(|s| self.window_for_surface(s))
            .as_ref()
            .and_then(Self::window_id);
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl SelectionHandler for DesktopState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for DesktopState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for DesktopState {}
impl ServerDndGrabHandler for DesktopState {}

impl OutputHandler for DesktopState {}

delegate_compositor!(DesktopState);
delegate_shm!(DesktopState);
delegate_xdg_shell!(DesktopState);
delegate_seat!(DesktopState);
delegate_data_device!(DesktopState);
delegate_output!(DesktopState);
