//! `_NET_WM_PING` handling for XWayland windows.
//!
//! smithay's `X11Wm` owns the window-manager role but exposes no API to ping
//! X11 clients or to observe their pong replies (pongs land in an internal
//! catch-all and are dropped). We therefore open a second, passive X11
//! connection to the same XWayland server. It never requests
//! SubstructureRedirect (which only the real WM may hold), so it does not
//! conflict with smithay; it only:
//!
//!  - selects SubstructureNotify on the root window, so `_NET_WM_PING` replies
//!    (which clients send to the root) are delivered to us, and
//!  - sends `_NET_WM_PING` ClientMessages to client windows.
//!
//! Only windows that advertise `_NET_WM_PING` in `WM_PROTOCOLS` are pinged
//! (typically GTK/Qt/Java apps); legacy X11 apps that don't are simply never
//! marked frozen.

use std::sync::Arc;

use smithay::reexports::x11rb::{
    self,
    connection::Connection,
    protocol::{
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConnectionExt, CreateWindowAux,
            EventMask, Window, WindowClass, CLIENT_MESSAGE_EVENT,
        },
        Event,
    },
    rust_connection::RustConnection,
};

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        WM_PROTOCOLS,
        _NET_WM_PING,
    }
}

pub struct X11Ping {
    conn: Arc<RustConnection>,
    atoms: Atoms,
}

impl X11Ping {
    /// Open the passive observer connection to `DISPLAY=:display`.
    ///
    /// Returns the ping handle and a shared connection clone to hand to
    /// [`smithay::utils::x11rb::X11Source`] for calloop integration.
    pub fn connect(display: u32) -> anyhow::Result<(Self, Arc<RustConnection>, Window)> {
        let (conn, screen_num) = RustConnection::connect(Some(&format!(":{display}")))?;
        let conn = Arc::new(conn);
        let atoms = Atoms::new(&*conn)?.reply()?;

        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow::anyhow!("XWayland screen {screen_num} missing"))?;
        let root = screen.root;

        // Observe substructure notifications on the root so `_NET_WM_PING`
        // replies reach us. Must NOT include SUBSTRUCTURE_REDIRECT.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?;

        // A minimal unmapped window; the X11Source reader thread is woken by a
        // ClientMessage to it when we shut down.
        let scratch = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            scratch,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            screen.root_visual,
            &CreateWindowAux::new(),
        )?;
        conn.flush()?;

        Ok((
            X11Ping {
                conn: conn.clone(),
                atoms,
            },
            conn,
            scratch,
        ))
    }

    /// Atom used as the wake-up ClientMessage type for the X11Source thread.
    pub fn wakeup_atom(&self) -> u32 {
        self.atoms.WM_PROTOCOLS
    }

    /// Whether the window advertises `_NET_WM_PING` support.
    pub fn supports_ping(&self, window: Window) -> bool {
        let Ok(cookie) =
            self.conn
                .get_property(false, window, self.atoms.WM_PROTOCOLS, AtomEnum::ATOM, 0, 64)
        else {
            return false;
        };
        match cookie.reply() {
            Ok(reply) => reply
                .value32()
                .map(|mut atoms| atoms.any(|a| a == self.atoms._NET_WM_PING))
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Send a `_NET_WM_PING` to the client window, using `serial` as the
    /// timestamp so the reply can be correlated. Returns false on send error.
    pub fn send_ping(&self, window: Window, serial: u32) -> bool {
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window,
            type_: self.atoms.WM_PROTOCOLS,
            data: [self.atoms._NET_WM_PING, serial, window, 0, 0].into(),
        };
        // Delivered directly to the client window (no propagation).
        self.conn
            .send_event(false, window, EventMask::NO_EVENT, event)
            .is_ok()
            && self.conn.flush().is_ok()
    }

    /// If `event` is a `_NET_WM_PING` reply, return `(client_window, serial)`.
    pub fn parse_pong(&self, event: &Event) -> Option<(Window, u32)> {
        if let Event::ClientMessage(msg) = event {
            if msg.type_ == self.atoms.WM_PROTOCOLS && msg.format == 32 {
                let data = msg.data.as_data32();
                if data[0] == self.atoms._NET_WM_PING {
                    // data[1] = our serial (timestamp), data[2] = client window
                    return Some((data[2], data[1]));
                }
            }
        }
        None
    }
}
