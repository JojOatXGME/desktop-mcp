# desktop-mcp

A headless Wayland compositor whose primary interface is an **MCP server**.
LLMs drive applications through MCP tools; every tool injects input, waits
until the UI settles, and returns the new UI state — window list, per-window
screenshots and AT-SPI accessibility metadata — so the model never has to ask
for screenshots separately. Humans can watch (read-only) through a built-in
web monitor.

Built with [smithay] (compositor), [rmcp] (MCP server, streamable HTTP) and
[atspi] (accessibility).

[smithay]: https://github.com/Smithay/smithay
[rmcp]: https://github.com/modelcontextprotocol/rust-sdk
[atspi]: https://github.com/odilia-app/atspi

## Build

Native dependencies (Debian/Ubuntu):

```sh
sudo apt-get install libxkbcommon-dev libpixman-1-dev libwayland-dev \
                     dbus at-spi2-core
# Optional, for running X11 apps through XWayland:
sudo apt-get install xwayland libegl1 libgl1
cargo build --release
```

## Run

Daemonized (dynamic Wayland socket, prints environment for the caller):

```sh
eval "$(desktop-mcp fork)"
echo "$DESKTOP_MCP_URL"       # http://127.0.0.1:8080/mcp
echo "$WAYLAND_DISPLAY"       # e.g. wayland-1
kill "$DESKTOP_MCP_PID"       # stop the daemon
```

Foreground (fixed socket name `wayland-mcp`):

```sh
desktop-mcp run
```

Options (both modes): `--port <p>` (default 8080), `--width/--height`
(default 1280×800), `--socket-name <name>`.

- MCP endpoint: `http://127.0.0.1:8080/mcp` (streamable HTTP)
- Human monitor: `http://127.0.0.1:8080/` (read-only live view)
- Daemon log: `$XDG_RUNTIME_DIR/desktop-mcp.log`

### Reaching the web UI from outside the container / WSL

The server binds to `127.0.0.1` inside the dev container. When the container
runs in WSL and VS Code's automatic port forwarding is not available, two
`socat` relays (run from Windows) bridge the gap:

```sh
# 1. Inside the container: expose 127.0.0.1:8080 on all container interfaces
wsl npx @devcontainers/cli exec socat TCP-LISTEN:9090,fork,bind=0.0.0.0 TCP:127.0.0.1:8080

# 2. Inside WSL: forward localhost:8080 to the container's IP
wsl socat TCP-LISTEN:8080,fork,bind=127.0.0.1 TCP:172.17.0.2:9090
```

The container IP (`172.17.0.2` above) can be looked up with:

```sh
wsl npx @devcontainers/cli exec ip addr
```

Windows forwards localhost requests into WSL automatically, so the monitor is
then reachable at `http://127.0.0.1:8080/` from the Windows browser.
(A more robust forwarding mechanism for running this setup outside VS Code is
an open follow-up.)

## MCP tools

| Tool | Purpose |
| --- | --- |
| `get_state` | Observe: window list + screenshots + accessibility |
| `wait` | Wait for the next screen (after loading indicators); settles once no update arrived for `settle_ms` (default 1.5 s), timeout `wait_ms` (default 60 s) |
| `click` / `mouse_down` / `mouse_up` / `mouse_move` | Pointer input |
| `scroll` | Wheel scrolling |
| `type_text` | Type text (`\n` = Return, `\t` = Tab) |
| `press_key` | Key/combos: `Return`, `ctrl+c`, `alt+F4`, … |
| `launch_app` | Start a program on this desktop (env prepared) |
| `focus_window` / `close_window` / `resize_window` | Window management |

Common parameters: `window` (makes x/y window-relative, matching that
window's screenshot and the accessibility `bounds`), `wait_ms` (convergence
timeout, default 3000, 0 = immediate), `watch_window` (only track this window
and new windows), `screenshots`, `accessibility` (default true).

Every result reports for each window: title, app id, geometry, `focused`,
`changed`, `new`, `frozen` (client unresponsive to pings for >10 s), `x11`
(window belongs to an X11 app running through XWayland), plus
`closed_windows` and `timed_out` for the transition.

X11 applications are supported transparently: XWayland is started
automatically (if the `Xwayland` binary is present) and `DISPLAY` is exported
alongside `WAYLAND_DISPLAY`, so both Wayland-native and X11 apps run on the
same virtual desktop.

## How convergence detection works

After the action is injected the compositor tracks surface commits and sends
xdg-shell pings to the relevant clients. The transition is complete after two
consecutive ping-pong rounds without any commit; if the UI keeps changing
(e.g. animations), the state is returned when `wait_ms` expires with
`timed_out: true`.

See `report.md` for design decisions and divergences from the original plan.
