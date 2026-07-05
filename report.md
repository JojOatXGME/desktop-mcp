# desktop-mcp — implementation report

A headless Wayland compositor (smithay) whose primary interface is an MCP
server (rmcp, streamable HTTP at `http://127.0.0.1:8080/mcp`). Every MCP tool
injects input, waits until the UI settles and returns the new UI state
(window list + per-window screenshot + AT-SPI accessibility metadata), so the
model never polls for screenshots. A human can watch the desktop read-only at
`http://127.0.0.1:8080/`.

Everything described in `promt.md` is implemented and was validated end-to-end
inside this dev container (foot terminal and zenity/GTK4, driven through the
MCP endpoint: launching apps, typing shell commands, clicking dialog buttons
via accessibility-reported coordinates, scrolling, key combos, resize/close,
daemonized startup via `eval "$(desktop-mcp fork)"`).

## Divergences from the plan (and why)

### 1. Human monitoring: web interface instead of a VNC server
The prompt offered "VNC server" or "noVNC-like webinterface". I implemented
the web interface: the HTTP server that already hosts `/mcp` also serves a
read-only monitor page at `/` (live desktop image at ~2 fps via `/frame.png`,
window list via `/state.json`, red crosshair marks the pointer).
**Reason:** a real RFB/VNC implementation would add a large protocol surface
or a C dependency for strictly debugging-only functionality, and VNC clients
expect to send input, which the design explicitly forbids. The web page is
read-only by construction and needs no extra software on the human's side.

### 2. Ping-pong tracking is per client, not per window
The prompt describes ping requests per window. The xdg-shell protocol only
provides ping/pong on `xdg_wm_base`, i.e. per client connection, not per
toplevel. Convergence detection therefore pings all *relevant* shell clients
(the watched window's client and clients of newly created windows, or all
clients when nothing specific is watched) and requires **two consecutive
ping-pong rounds without any surface commit** (rounds evaluated every 50 ms).
"Frozen" (no pong for >10 s, checked continuously via a 3 s heartbeat) is
likewise a per-client property reported on each of the client's windows.

### 3. Additional MCP tools beyond the prompt's list
Implemented: `get_state`, `click`, `mouse_down`, `mouse_up`, `mouse_move`,
`scroll`, `type_text`, `press_key` — plus `launch_app`, `focus_window`,
`close_window`, `resize_window`.
**Reason:** without `launch_app` the model has no way to start applications
with the right environment (`WAYLAND_DISPLAY`, D-Bus, accessibility vars).
The window-management tools map to xdg-shell requests the compositor must
support anyway and make common flows cheap.

### 4. Screenshots are per window and coordinates are window-relative
The prompt left open whether to screenshot the whole desktop or windows. Each
reported window gets its own screenshot cropped to its geometry, and all
tools accept a `window` parameter that makes x/y relative to that window
(matching the screenshot pixels and the accessibility `bounds`). Global
desktop coordinates work too (omit `window`). The full desktop image exists
only on the monitor page.
**Reason:** window-relative coordinates remove one mental transformation for
the model and stay valid when windows move.

### 5. Fixed HTTP port, dynamic Wayland socket
`fork` mode allocates the Wayland socket dynamically (`wayland-0`, `wayland-1`,
…) and prints the environment for `eval`, as requested. The MCP/monitor HTTP
port however defaults to fixed `8080` (configurable with `--port`).
**Reason:** the validation MCP client was preconfigured for
`http://127.0.0.1:8080/mcp`; a dynamically allocated port would break any
pre-registered MCP configuration. `run` mode uses the fixed socket name
`wayland-mcp` by default (the prompt's "non-forking mode without dynamic
allocation"); `--socket-name` overrides in both modes.

### 6. Display resolution
Default virtual output is **1280×800@60** (configurable `--width/--height`).
"A resolution matching the LLM's visual capabilities" — 1280×800 ≈ 1 MP stays
below typical vision-model downscaling thresholds while fitting real
application UIs.

### 7. Typing is limited to keysyms reachable through the US keymap
`type_text` maps characters to keycodes of the "us" keymap (all ASCII incl.
shifted symbols; `\n`/`\t` become Return/Tab). Characters outside the keymap
(e.g. umlauts, CJK) are skipped and reported in the `warnings` field of the
result.
**Reason:** arbitrary Unicode input would require rebuilding the xkb keymap on
the fly or a text-input protocol implementation; not worth it for v1 and the
limitation is surfaced explicitly to the model.

### 8. Accessibility integration details
`desktop-mcp` spawns a **private D-Bus session bus** plus
`at-spi-bus-launcher` (from `at-spi2-core`) and exports
`DBUS_SESSION_BUS_ADDRESS`, `GTK_A11Y=atspi`,
`QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1` to everything launched on the desktop.
Accessible trees are read via the `atspi` crate, attributed to windows by
**process id** (and window title when one app has several frames), and
attached to the window state as `accessibility` (role, name, description,
selected states, and window-relative pixel `bounds` per element, capped at
250 nodes / depth 10 per window). Validated with GTK4 (zenity): the model can
click buttons using the reported `bounds` directly.
Caveats: GTK3 apps additionally need the `libatk-adaptor` package; apps
without AT-SPI support simply yield no `accessibility` field (best-effort by
design).

### 9. No XWayland support
Only Wayland-native clients are supported. XWayland would pull in an X11
stack and rootful/rootless window integration; out of scope for v1. Modern
GTK/Qt/SDL/winit apps all speak Wayland natively.

### 10. Popup grabs are not implemented
`xdg_popup.grab` is acknowledged but no grab semantics are enforced. Popups
(menus, tooltips) still render, receive pointer input through normal focus
resolution, and report as changes on their parent window. Grab semantics
matter for real users with real seats; for injected input they add nothing.

### 11. The three share.google links could not be read
The `share.google/aimode/…` links in `promt.md` are JavaScript-gated Google
"AI mode" shares that cannot be fetched from this environment. The design was
implemented from the smithay 0.7 / rmcp 2.1 / atspi 0.30 sources and docs
instead.

### 12. `apt` works with sudo
`apt` failed only because of missing root rights; `sudo apt-get` works in
this container. Installed for build and testing: `libxkbcommon-dev`,
`libpixman-1-dev`, `libwayland-dev`, `dbus`, `at-spi2-core`, `wayland-utils`,
`fonts-dejavu-core`, and the test apps `foot` and `zenity`.

## Design notes (not divergences)

- **Threading:** the compositor runs a calloop event loop on the main thread;
  rmcp/axum run on a tokio thread. MCP tools send a request (action + wait
  parameters + oneshot reply) over a calloop channel; the compositor performs
  the action, tracks the transition and replies with the snapshot when the UI
  converged (or the configurable `wait_ms` timeout, default 3000 ms, hit —
  reported as `timed_out: true`).
- **Rendering** is fully software (smithay's pixman renderer): a ~30 fps tick
  renders the desktop for the monitor and delivers frame callbacks;
  per-window screenshots render the window's surface tree (including popups)
  into an offscreen pixman buffer, encoded as PNG.
- **Change tracking:** every surface commit is attributed to its toplevel
  window (subsurfaces and popups map to their ancestor). Windows created or
  destroyed during a transition are reported (`new`, `closed_windows`).
  Continuously animating apps therefore never "converge" and return at the
  timeout — that is the prompt's intended step-4 behaviour.
- **launch_app** waits for the first *new window* before the quiet-round rule
  may finish the wait, so slow app startups don't return an empty desktop.
- `default_wait` for `get_state` is 1500 ms since it usually observes an idle
  desktop; interaction tools default to 3000 ms as specified; every tool
  accepts `wait_ms` (0 = immediate snapshot).

## Validation performed

1. `cargo build` — clean (no warnings).
2. `eval "$(desktop-mcp fork)"` returns immediately with
   `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`,
   `DESKTOP_MCP_URL`, `DESKTOP_MCP_MONITOR_URL`, `DESKTOP_MCP_PID` exports;
   daemon logs to `$XDG_RUNTIME_DIR/desktop-mcp.log`.
3. MCP handshake + `tools/list` against `http://127.0.0.1:8080/mcp`.
4. `launch_app foot` → window appears with screenshot; `type_text` executed a
   shell command (screenshot shows the output); `scroll` moved the scrollback;
   `press_key` combos accepted.
5. `launch_app zenity --question` → GTK4 dialog; accessibility tree contains
   the Yes/No buttons with window-relative `bounds`; `click` at the reported
   bounds closed the dialog and the result reported `closed_windows`.
6. `resize_window` (client resized to 900×598 — cell-rounded by foot) and
   `close_window` verified.
7. Monitor page at `http://127.0.0.1:8080/` shows the live desktop with
   pointer crosshair and window list.
