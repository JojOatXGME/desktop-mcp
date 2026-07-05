//! desktop-mcp: a headless Wayland compositor whose primary interface is an
//! MCP server. LLMs interact with applications through MCP tools; humans can
//! watch (read-only) through a built-in web monitor.

mod a11y;
mod comp;
mod daemon;
mod ipc;
mod keys;
mod mcp;
mod monitor;
mod render;
mod tracker;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use smithay::reexports::calloop::{
    channel,
    generic::Generic,
    timer::{TimeoutAction, Timer},
    EventLoop, Interest, Mode, PostAction,
};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;

use comp::{ClientState, DesktopState};
use mcp::EnvInfo;
use render::FrameStore;

#[derive(Parser)]
#[command(
    name = "desktop-mcp",
    about = "Headless Wayland compositor controlled through an MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start as a background daemon and print shell exports; use with
    /// `eval "$(desktop-mcp fork)"`.
    Fork(Opts),
    /// Run in the foreground (uses a fixed Wayland socket name by default).
    Run(Opts),
}

#[derive(Args, Clone)]
struct Opts {
    /// TCP port for the MCP server and web monitor (127.0.0.1).
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Virtual screen width in pixels.
    #[arg(long, default_value_t = 1280)]
    width: i32,
    /// Virtual screen height in pixels.
    #[arg(long, default_value_t = 800)]
    height: i32,
    /// Fixed Wayland socket name. Default: automatic (wayland-N) in fork
    /// mode, "wayland-mcp" in run mode.
    #[arg(long)]
    socket_name: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(opts) => {
            init_tracing();
            let runtime_dir = ensure_runtime_dir()?;
            let opts = Opts {
                socket_name: opts.socket_name.clone().or(Some("wayland-mcp".into())),
                ..opts
            };
            run_server(opts, runtime_dir, None)
        }
        Command::Fork(opts) => {
            let runtime_dir = ensure_runtime_dir()?;
            match daemon::fork_daemon()? {
                daemon::ForkOutcome::Parent(pipe) => daemon::parent_wait_and_print(pipe),
                daemon::ForkOutcome::Child(pipe) => {
                    let log_path = runtime_dir.join("desktop-mcp.log");
                    daemon::redirect_stdio(&log_path)?;
                    init_tracing();
                    tracing::info!("daemon started, logging to {}", log_path.display());
                    run_server(opts, runtime_dir, Some(pipe))
                }
            }
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

/// Make sure XDG_RUNTIME_DIR points at a writable private directory (the
/// Wayland socket and D-Bus sockets live there).
fn ensure_runtime_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(&dir);
        if path.is_dir()
            && std::fs::metadata(&path)
                .map(|m| !m.permissions().readonly())
                .unwrap_or(false)
        {
            return Ok(path);
        }
    }
    let uid = unsafe { libc::getuid() };
    let path = PathBuf::from(format!("/tmp/desktop-mcp-{uid}"));
    std::fs::create_dir_all(&path)?;
    let mut perms = std::fs::metadata(&path)?.permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o700);
    std::fs::set_permissions(&path, perms)?;
    // SAFETY: single-threaded startup.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &path) };
    Ok(path)
}

fn run_server(
    opts: Opts,
    runtime_dir: PathBuf,
    ready_pipe: Option<std::fs::File>,
) -> anyhow::Result<()> {
    // Auto-reap applications spawned via the launch_app tool.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };

    // Accessibility infrastructure (private session bus + at-spi). Optional.
    let a11y_buses = match a11y::spawn_buses() {
        Ok(buses) => Some(buses),
        Err(e) => {
            tracing::warn!("accessibility buses unavailable: {e}");
            None
        }
    };

    let mut event_loop: EventLoop<DesktopState> =
        EventLoop::try_new().context("failed to create event loop")?;
    let display: Display<DesktopState> = Display::new().context("failed to create display")?;
    let display_handle = display.handle();

    let frames = Arc::new(FrameStore::new());
    let mut state = DesktopState::new(
        display_handle.clone(),
        event_loop.handle(),
        (opts.width, opts.height),
        frames.clone(),
    )?;

    // Wayland listening socket.
    let socket = match &opts.socket_name {
        Some(name) => ListeningSocketSource::with_name(name),
        None => ListeningSocketSource::new_auto(),
    }
    .context("failed to bind Wayland socket")?;
    let socket_name = socket.socket_name().to_string_lossy().to_string();
    // Point every toolkit at this compositor. Without the explicit backend
    // overrides, Qt defaults to X11 (xcb), SDL2 prefers X11, and GTK could
    // pick a foreign X server if DISPLAY is set (e.g. WSLg).
    // SAFETY: no other threads are running yet.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name);
        std::env::set_var("GDK_BACKEND", "wayland");
        std::env::set_var("QT_QPA_PLATFORM", "wayland");
        std::env::set_var("SDL_VIDEODRIVER", "wayland");
        // We don't implement xdg-decoration, so clients must draw their own
        // decorations.
        std::env::set_var("GTK_CSD", "1");
        // Keep clients' own keymap fallbacks in sync with the compositor
        // keymap (input injection assumes the us layout).
        std::env::set_var("XKB_DEFAULT_LAYOUT", "us");
        std::env::remove_var("DISPLAY");
    }

    let handle = event_loop.handle();
    handle
        .insert_source(socket, |client_stream, _, state| {
            if let Err(e) = state
                .display_handle
                .insert_client(client_stream, Arc::new(ClientState::default()))
            {
                tracing::warn!("failed to insert client: {e}");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: the display is not moved out of the source.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert display source: {e}"))?;

    // Requests from the MCP/HTTP layer.
    let (req_tx, req_rx) = channel::channel::<ipc::Request>();
    handle
        .insert_source(req_rx, |event, _, state| {
            if let channel::Event::Msg(req) = event {
                state.handle_request(req);
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert request channel: {e}"))?;

    // ~30 fps render tick (frame callbacks + monitor frame).
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(33)),
            |_, _, state| {
                render_tick(state);
                TimeoutAction::ToDuration(Duration::from_millis(33))
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert render timer: {e}"))?;

    // Transition tracker driver.
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(50)),
            |_, _, state| {
                if !state.waits.is_empty() {
                    state.tracker_tick();
                }
                TimeoutAction::ToDuration(Duration::from_millis(50))
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert tracker timer: {e}"))?;

    // Freeze-detection heartbeat.
    handle
        .insert_source(
            Timer::from_duration(Duration::from_secs(3)),
            |_, _, state| {
                state.heartbeat();
                TimeoutAction::ToDuration(Duration::from_secs(3))
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert heartbeat timer: {e}"))?;

    // HTTP layer (MCP + monitor) on its own tokio thread.
    let mcp_url = format!("http://127.0.0.1:{}/mcp", opts.port);
    let monitor_url = format!("http://127.0.0.1:{}/", opts.port);
    let env_info = EnvInfo {
        wayland_display: socket_name.clone(),
        mcp_url: mcp_url.clone(),
        monitor_url: monitor_url.clone(),
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    {
        let frames = frames.clone();
        let req_tx = req_tx.clone();
        let env_info = env_info.clone();
        let port = opts.port;
        std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(async move {
                    let a11y = Arc::new(
                        match tokio::time::timeout(Duration::from_secs(5), a11y::A11y::connect())
                            .await
                        {
                            Ok(Ok(a)) => Some(a),
                            Ok(Err(e)) => {
                                tracing::warn!("accessibility connection failed: {e}");
                                None
                            }
                            Err(_) => {
                                tracing::warn!("accessibility connection timed out");
                                None
                            }
                        },
                    );
                    let app = monitor::router(frames, req_tx, a11y, env_info);
                    let listener =
                        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                            Ok(l) => l,
                            Err(e) => {
                                let _ = ready_tx.send(Err(format!("bind port {port}: {e}")));
                                return;
                            }
                        };
                    let _ = ready_tx.send(Ok(()));
                    if let Err(e) = axum::serve(listener, app).await {
                        tracing::error!("http server failed: {e}");
                    }
                });
            })?;
    }

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            report_failure(ready_pipe, &e);
            anyhow::bail!("startup failed: {e}");
        }
        Err(_) => {
            report_failure(ready_pipe, "http thread did not report readiness");
            anyhow::bail!("startup failed: http thread did not report readiness");
        }
    }

    // Report the environment.
    let mut exports = String::new();
    let mut add = |k: &str, v: &str| {
        exports.push_str(&format!("export {}='{}'\n", k, v.replace('\'', r"'\''")));
    };
    add("WAYLAND_DISPLAY", &socket_name);
    add("XDG_RUNTIME_DIR", &runtime_dir.to_string_lossy());
    add("GDK_BACKEND", "wayland");
    add("QT_QPA_PLATFORM", "wayland");
    add("SDL_VIDEODRIVER", "wayland");
    add("GTK_CSD", "1");
    add("XKB_DEFAULT_LAYOUT", "us");
    if let Some(buses) = &a11y_buses {
        add("DBUS_SESSION_BUS_ADDRESS", &buses.session_bus_address);
        add("GTK_A11Y", "atspi");
        add("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1");
        add("ASSISTIVE_TECHNOLOGIES", "org.GNOME.Accessibility.AtkWrapper");
    }
    add("DESKTOP_MCP_URL", &mcp_url);
    add("DESKTOP_MCP_MONITOR_URL", &monitor_url);
    add("DESKTOP_MCP_PID", &std::process::id().to_string());

    match ready_pipe {
        Some(mut pipe) => {
            // fork mode: hand the exports to the waiting parent.
            let _ = pipe.write_all(exports.as_bytes());
            drop(pipe);
        }
        None => {
            println!("{exports}");
        }
    }
    tracing::info!(
        "desktop-mcp ready: WAYLAND_DISPLAY={socket_name}, mcp={mcp_url}, monitor={monitor_url}"
    );

    event_loop
        .run(None, &mut state, |state| {
            state.space.refresh();
            state.popups.cleanup();
            let _ = state.display_handle.flush_clients();
        })
        .context("event loop failed")?;

    drop(a11y_buses);
    Ok(())
}

fn report_failure(ready_pipe: Option<std::fs::File>, msg: &str) {
    if let Some(mut pipe) = ready_pipe {
        let _ = writeln!(pipe, "# desktop-mcp startup failed: {msg}");
    }
}

/// Render the desktop for the monitor and deliver frame callbacks so clients
/// keep animating.
fn render_tick(state: &mut DesktopState) {
    if let Err(e) = render::render_desktop_frame(
        &mut state.renderer,
        &state.space,
        &state.output,
        &mut state.damage_tracker,
        state.screen_size,
        state.pointer_loc,
        &state.frames,
    ) {
        tracing::warn!("render failed: {e}");
    }
    let elapsed = state.start_time.elapsed();
    let output = state.output.clone();
    for window in state.space.elements() {
        window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
        if let Some(toplevel) = window.toplevel() {
            for (popup, _) in
                smithay::desktop::PopupManager::popups_for_surface(toplevel.wl_surface())
            {
                smithay::desktop::utils::send_frames_surface_tree(
                    popup.wl_surface(),
                    &output,
                    elapsed,
                    Some(Duration::ZERO),
                    |_, _| Some(output.clone()),
                );
            }
        }
    }
    let _ = state.display_handle.flush_clients();
}
