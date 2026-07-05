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

## MCP tools

| Tool | Purpose |
| --- | --- |
| `get_state` | Observe: window list + screenshots + accessibility |
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
`changed`, `new`, `frozen` (client unresponsive to pings for >10 s), plus
`closed_windows` and `timed_out` for the transition.

## How convergence detection works

After the action is injected the compositor tracks surface commits and sends
xdg-shell pings to the relevant clients. The transition is complete after two
consecutive ping-pong rounds without any commit; if the UI keeps changing
(e.g. animations), the state is returned when `wait_ms` expires with
`timed_out: true`.

See `report.md` for design decisions and divergences from the original plan.
