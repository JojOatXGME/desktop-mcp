//! Transition tracking: after an action is injected, wait until the UI has
//! settled. A transition is considered complete once two consecutive
//! ping-pong rounds to the relevant clients passed without any surface
//! commits. Clients whose ping goes unanswered for more than 10 seconds are
//! reported as frozen.
//!
//! Wayland clients are pinged through xdg-shell; XWayland windows through
//! `_NET_WM_PING` (see [`crate::x11ping`]). Both feed the same bookkeeping,
//! keyed by [`PingKey`].

use std::{
    collections::HashSet,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use smithay::{utils::SERIAL_COUNTER, wayland::shell::xdg::ShellClient};
use tokio::sync::oneshot;

use crate::{
    comp::{shell_client_key, DesktopState},
    ipc::{DesktopSnapshot, SnapshotRequest, WaitParams},
};

pub const FROZEN_AFTER: Duration = Duration::from_secs(10);
/// How many quiet ping-pong rounds mark a transition as complete.
const QUIET_ROUNDS: u32 = 2;
/// Minimum quiet period for waits involving X11 windows that cannot be pinged
/// (they don't advertise `_NET_WM_PING`). Long enough to catch a typical
/// redraw after injected input.
const X11_SETTLE_GRACE: Duration = Duration::from_millis(400);

/// Identifies a pingable client: a Wayland xdg shell client, or an XWayland
/// client window pinged via `_NET_WM_PING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PingKey {
    Wayland(u64),
    X11(u32),
}

static X11_PING_SERIAL: AtomicU32 = AtomicU32::new(1);

#[derive(Debug)]
pub struct PingState {
    /// Serial and send time of a ping that has not been answered yet.
    pub outstanding: Option<(u32, Instant)>,
    pub last_pong: Instant,
}

pub struct Wait {
    pub deadline: Instant,
    pub watch: Option<u64>,
    pub expect_new_window: bool,
    pub known_at_start: HashSet<u64>,
    pub changed: HashSet<u64>,
    pub closed: Vec<u64>,
    pub quiet_rounds: u32,
    /// Ping keys the current round is waiting on; None = no round in flight.
    pub round: Option<Vec<PingKey>>,
    pub round_dirty: bool,
    /// Time-based settling (the `wait` tool): settled once this much time
    /// passed without updates. None = two-quiet-rounds rule.
    pub quiet_time: Option<Duration>,
    /// Don't finish before at least one update was observed.
    pub require_activity: bool,
    pub has_activity: bool,
    pub last_activity: Instant,
    pub snapshot: SnapshotRequest,
    pub reply: Option<oneshot::Sender<Result<DesktopSnapshot, String>>>,
    pub warnings: Vec<String>,
}

impl DesktopState {
    pub fn start_wait(
        &mut self,
        params: WaitParams,
        snapshot: SnapshotRequest,
        reply: oneshot::Sender<Result<DesktopSnapshot, String>>,
        warnings: Vec<String>,
    ) {
        let wait = Wait {
            deadline: Instant::now() + Duration::from_millis(params.timeout_ms),
            watch: params.watch_window,
            expect_new_window: params.expect_new_window,
            known_at_start: self.windows.keys().copied().collect(),
            changed: HashSet::new(),
            closed: Vec::new(),
            quiet_rounds: 0,
            round: None,
            round_dirty: false,
            quiet_time: params.quiet_time_ms.map(Duration::from_millis),
            require_activity: params.require_activity,
            has_activity: false,
            last_activity: Instant::now(),
            snapshot,
            reply: Some(reply),
            warnings,
        };
        self.waits.push(wait);
        self.tracker_tick();
    }

    /// A window committed something (or appeared/closed a popup).
    pub fn note_window_activity(&mut self, id: u64) {
        for wait in &mut self.waits {
            let relevant = match wait.watch {
                None => true,
                Some(watched) => watched == id || !wait.known_at_start.contains(&id),
            };
            if relevant {
                wait.changed.insert(id);
                wait.round_dirty = true;
                wait.has_activity = true;
                wait.last_activity = Instant::now();
            }
        }
    }

    pub fn note_window_closed(&mut self, id: u64) {
        for wait in &mut self.waits {
            let relevant = match wait.watch {
                None => true,
                Some(watched) => watched == id || !wait.known_at_start.contains(&id),
            };
            if relevant {
                wait.closed.push(id);
                wait.round_dirty = true;
                wait.has_activity = true;
                wait.last_activity = Instant::now();
            }
        }
    }

    /// A Wayland xdg client answered our ping. The xdg pong carries no serial
    /// we tracked, so any pong clears the outstanding ping (as before).
    pub fn note_pong(&mut self, key: u64) {
        let now = Instant::now();
        let entry = self.ping_states.entry(PingKey::Wayland(key)).or_insert(PingState {
            outstanding: None,
            last_pong: now,
        });
        entry.outstanding = None;
        entry.last_pong = now;
        self.evaluate_waits();
    }

    /// An XWayland window answered a `_NET_WM_PING`. Only clears the ping if
    /// the serial matches, so stale replies don't end a round early.
    pub fn note_x11_pong(&mut self, window: u32, serial: u32) {
        let now = Instant::now();
        if let Some(entry) = self.ping_states.get_mut(&PingKey::X11(window)) {
            if entry.outstanding.map(|(s, _)| s == serial).unwrap_or(false) {
                entry.outstanding = None;
                entry.last_pong = now;
            }
        }
        self.evaluate_waits();
    }

    pub fn client_frozen(&self, key: PingKey) -> bool {
        self.ping_states
            .get(&key)
            .and_then(|p| p.outstanding)
            .map(|(_, sent)| sent.elapsed() > FROZEN_AFTER)
            .unwrap_or(false)
    }

    /// Shell clients relevant for a wait: the watched window's client plus
    /// clients of windows created after the wait started — or all clients if
    /// nothing specific is watched.
    fn wait_targets(&self, watch: Option<u64>, known_at_start: &HashSet<u64>) -> Vec<ShellClient> {
        let mut clients = Vec::new();
        let mut seen = HashSet::new();
        for (id, window) in &self.windows {
            let relevant = match watch {
                None => true,
                Some(watched) => *id == watched || !known_at_start.contains(id),
            };
            if !relevant {
                continue;
            }
            if let Some(toplevel) = window.toplevel() {
                let sc = toplevel.client();
                if let Some(key) = shell_client_key(&sc) {
                    if seen.insert(key) {
                        clients.push(sc);
                    }
                }
            }
        }
        clients
    }

    /// X11 client windows relevant for a wait that advertise `_NET_WM_PING`.
    /// Support is queried once per window and cached.
    fn wait_x11_targets(&mut self, watch: Option<u64>, known_at_start: &HashSet<u64>) -> Vec<u32> {
        // Collect candidate (id, x11 window) pairs first to avoid holding a
        // borrow of self.windows while mutating the support cache.
        let candidates: Vec<u32> = self
            .windows
            .iter()
            .filter_map(|(id, window)| {
                let relevant = match watch {
                    None => true,
                    Some(watched) => *id == watched || !known_at_start.contains(id),
                };
                if !relevant {
                    return None;
                }
                window.x11_surface().map(|s| s.window_id())
            })
            .collect();

        let mut targets = Vec::new();
        for win in candidates {
            let supported = match self.x11_ping_support.get(&win) {
                Some(&s) => s,
                None => {
                    let s = self
                        .x11ping
                        .as_ref()
                        .map(|p| p.supports_ping(win))
                        .unwrap_or(false);
                    self.x11_ping_support.insert(win, s);
                    s
                }
            };
            if supported {
                targets.push(win);
            }
        }
        targets
    }

    /// Whether any window relevant to this wait is an X11 window that we can't
    /// ping (no `_NET_WM_PING`). Such waits fall back to a settle grace, since
    /// the ping-round rule would otherwise complete before the app reacts.
    fn wait_involves_unpingable_x11(
        &mut self,
        watch: Option<u64>,
        known_at_start: &HashSet<u64>,
    ) -> bool {
        let candidates: Vec<u32> = self
            .windows
            .iter()
            .filter_map(|(id, window)| {
                let relevant = match watch {
                    None => true,
                    Some(watched) => *id == watched || !known_at_start.contains(id),
                };
                if !relevant {
                    return None;
                }
                window.x11_surface().map(|s| s.window_id())
            })
            .collect();
        candidates.into_iter().any(|win| {
            let supported = match self.x11_ping_support.get(&win) {
                Some(&s) => s,
                None => {
                    let s = self
                        .x11ping
                        .as_ref()
                        .map(|p| p.supports_ping(win))
                        .unwrap_or(false);
                    self.x11_ping_support.insert(win, s);
                    s
                }
            };
            !supported
        })
    }

    /// Send a ping to the given shell client unless one is already in flight.
    fn ensure_ping(&mut self, sc: &ShellClient) -> Option<PingKey> {
        let key = PingKey::Wayland(shell_client_key(sc)?);
        let now = Instant::now();
        let entry = self.ping_states.entry(key).or_insert(PingState {
            outstanding: None,
            last_pong: now,
        });
        if entry.outstanding.is_none() {
            let serial = SERIAL_COUNTER.next_serial();
            // A failure is either a ping we already sent (keep waiting on the
            // existing entry) or a dead client (nothing to wait for).
            if sc.send_ping(serial).is_ok() {
                entry.outstanding = Some((u32::from(serial), now));
            } else if !sc.alive() {
                return None;
            }
        }
        Some(key)
    }

    /// Send a `_NET_WM_PING` to an X11 window unless one is already in flight.
    fn ensure_x11_ping(&mut self, window: u32) -> Option<PingKey> {
        let x11ping = self.x11ping.as_ref()?;
        let now = Instant::now();
        let key = PingKey::X11(window);
        let entry = self.ping_states.entry(key).or_insert(PingState {
            outstanding: None,
            last_pong: now,
        });
        if entry.outstanding.is_none() {
            let serial = X11_PING_SERIAL.fetch_add(1, Ordering::Relaxed);
            if x11ping.send_ping(window, serial) {
                entry.outstanding = Some((serial, now));
            }
        }
        Some(key)
    }

    /// Periodic driver, called from a calloop timer (~every 50ms) and after
    /// pongs. Starts ping rounds, checks completion and deadlines.
    pub fn tracker_tick(&mut self) {
        let now = Instant::now();
        // start rounds where none is in flight
        for i in 0..self.waits.len() {
            if self.waits[i].round.is_none() && now < self.waits[i].deadline {
                let (watch, known) = {
                    let w = &self.waits[i];
                    (w.watch, w.known_at_start.clone())
                };
                let targets = self.wait_targets(watch, &known);
                let mut keys = Vec::new();
                for sc in &targets {
                    if let Some(key) = self.ensure_ping(sc) {
                        keys.push(key);
                    }
                }
                for win in self.wait_x11_targets(watch, &known) {
                    if let Some(key) = self.ensure_x11_ping(win) {
                        keys.push(key);
                    }
                }
                let wait = &mut self.waits[i];
                wait.round = Some(keys);
                wait.round_dirty = false;
            }
        }
        let _ = self.display_handle.flush_clients();
        self.evaluate_waits();
    }

    fn evaluate_waits(&mut self) {
        let now = Instant::now();
        let current_ids: HashSet<u64> = self.windows.keys().copied().collect();
        // Precompute, per wait, whether an X11 window that we cannot ping is
        // involved — those fall back to a settle grace. (Borrow of self ends
        // before the mutable loop below.)
        let watch_known: Vec<(Option<u64>, HashSet<u64>)> = self
            .waits
            .iter()
            .map(|w| (w.watch, w.known_at_start.clone()))
            .collect();
        let grace_needed: Vec<bool> = watch_known
            .into_iter()
            .map(|(watch, known)| self.wait_involves_unpingable_x11(watch, &known))
            .collect();
        let mut finished: Vec<(usize, bool)> = Vec::new();
        for (i, wait) in self.waits.iter_mut().enumerate() {
            if now >= wait.deadline {
                finished.push((i, true));
                continue;
            }
            if wait.expect_new_window && current_ids.is_subset(&wait.known_at_start) {
                // Still waiting for the launched app's first window.
                wait.round = None;
                continue;
            }
            if let Some(round) = &wait.round {
                let complete = round.iter().all(|key| {
                    self.ping_states
                        .get(key)
                        .map(|p| p.outstanding.is_none())
                        .unwrap_or(true)
                });
                if complete {
                    if wait.round_dirty {
                        wait.quiet_rounds = 0;
                    } else {
                        wait.quiet_rounds += 1;
                    }
                    wait.round = None;
                    wait.round_dirty = false;
                    let settled = match wait.quiet_time {
                        // Interaction tools: two quiet ping-pong rounds.
                        None => wait.quiet_rounds >= QUIET_ROUNDS,
                        // The `wait` tool: a quiet *period* (with working
                        // ping-pongs), after at least one observed update.
                        Some(quiet_time) => {
                            (!wait.require_activity || wait.has_activity)
                                && wait.last_activity.elapsed() >= quiet_time
                        }
                    };
                    // Unpingable X11 windows get a grace period after the last
                    // update (or after the action, if they never reacted)
                    // before the transition is considered complete. Pingable
                    // windows rely on the ping-pong rounds above instead.
                    let grace_ok = !grace_needed[i]
                        || wait.last_activity.elapsed() >= X11_SETTLE_GRACE;
                    if settled && grace_ok {
                        finished.push((i, false));
                    }
                }
            }
        }
        // finish in reverse index order so removals don't shift pending ones
        for (i, timed_out) in finished.into_iter().rev() {
            let mut wait = self.waits.remove(i);
            let reply = wait.reply.take();
            let snap = self.build_snapshot(
                &wait.snapshot.include,
                wait.snapshot.screenshots,
                &wait.changed,
                Some(&wait.known_at_start),
                &wait.closed,
                timed_out,
                std::mem::take(&mut wait.warnings),
            );
            if let Some(reply) = reply {
                let _ = reply.send(Ok(snap));
            }
        }
    }

    /// Ping all clients periodically so frozen windows are detected even
    /// while no transition is being tracked.
    pub fn heartbeat(&mut self) {
        let clients = self.wait_targets(None, &HashSet::new());
        for sc in &clients {
            self.ensure_ping(sc);
        }
        let _ = self.display_handle.flush_clients();
        for win in self.wait_x11_targets(None, &HashSet::new()) {
            self.ensure_x11_ping(win);
        }
    }
}
