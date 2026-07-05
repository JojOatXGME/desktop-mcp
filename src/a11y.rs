//! Accessibility (AT-SPI2) integration.
//!
//! We spawn a private D-Bus session bus and the at-spi bus launcher, export
//! the relevant environment variables (so every app we launch connects to
//! them), and read the accessible tree of each application to enrich window
//! state with UI metadata.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{CoordType, Interface, Role};
use serde_json::{json, Value};

const MAX_NODES: usize = 250;
const MAX_DEPTH: usize = 10;

/// Handles to the spawned bus daemons; killed on drop.
pub struct A11yBuses {
    pub session_bus_address: String,
    children: Vec<Child>,
}

impl Drop for A11yBuses {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn a private session D-Bus and the at-spi infrastructure on top of it.
/// Sets DBUS_SESSION_BUS_ADDRESS and accessibility env vars for this process
/// (and therefore for every child we spawn later).
pub fn spawn_buses() -> anyhow::Result<A11yBuses> {
    let mut dbus = Command::new("dbus-daemon")
        .args(["--session", "--print-address", "--nofork"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn dbus-daemon: {e}"))?;
    let stdout = dbus.stdout.take().unwrap();
    let mut address = String::new();
    BufReader::new(stdout).read_line(&mut address)?;
    let address = address.trim().to_string();
    if address.is_empty() {
        let _ = dbus.kill();
        anyhow::bail!("dbus-daemon did not report an address");
    }

    // Everything we (and our children) do from now on uses this session bus.
    // Safety: called during single-threaded startup, before tokio/threads.
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &address);
        std::env::set_var("GTK_A11Y", "atspi");
        std::env::set_var("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1");
        // Java Swing publishes its accessible tree only with the ATK wrapper
        // enabled.
        std::env::set_var("ASSISTIVE_TECHNOLOGIES", "org.GNOME.Accessibility.AtkWrapper");
        std::env::remove_var("NO_AT_BRIDGE");
    }

    let mut children = vec![dbus];

    // The launcher registers org.a11y.Bus on the session bus, starts the
    // dedicated accessibility bus and at-spi2-registryd.
    let launcher = ["/usr/libexec/at-spi-bus-launcher", "/usr/lib/at-spi2-core/at-spi-bus-launcher"]
        .iter()
        .find(|p| std::path::Path::new(p).exists());
    match launcher {
        Some(path) => {
            let child = Command::new(path)
                .arg("--launch-immediately")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| anyhow::anyhow!("failed to spawn at-spi-bus-launcher: {e}"))?;
            children.push(child);
        }
        None => tracing::warn!("at-spi-bus-launcher not found; accessibility metadata disabled"),
    }

    Ok(A11yBuses {
        session_bus_address: address,
        children,
    })
}

/// Async handle used by the MCP layer to query accessible trees.
pub struct A11y {
    conn: AccessibilityConnection,
}

impl A11y {
    pub async fn connect() -> anyhow::Result<Self> {
        // Flip the org.a11y.Status IsEnabled flag so toolkits enable their
        // accessibility bridge.
        if let Err(e) = atspi::connection::set_session_accessibility(true).await {
            tracing::warn!("could not enable session accessibility: {e}");
        }
        let conn = AccessibilityConnection::new().await?;
        Ok(A11y { conn })
    }

    /// Snapshot the accessible trees of all applications, grouped per
    /// top-level frame. Returns pid -> [window trees], plus trees that could
    /// not be attributed to a pid under key 0.
    pub async fn snapshot(&self) -> HashMap<i32, Vec<Value>> {
        match tokio::time::timeout(Duration::from_secs(4), self.snapshot_inner()).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!("a11y snapshot failed: {e}");
                HashMap::new()
            }
            Err(_) => {
                tracing::warn!("a11y snapshot timed out");
                HashMap::new()
            }
        }
    }

    async fn snapshot_inner(&self) -> anyhow::Result<HashMap<i32, Vec<Value>>> {
        let mut result: HashMap<i32, Vec<Value>> = HashMap::new();
        let registry = self.conn.root_accessible_on_registry().await?;
        let apps = registry.get_children().await?;
        let dbus = zbus::fdo::DBusProxy::new(self.conn.connection()).await?;
        for app_ref in apps {
            let bus_name = app_ref.name_as_str().unwrap_or_default().to_string();
            let pid = match zbus::names::BusName::try_from(bus_name.clone()) {
                Ok(name) => dbus
                    .get_connection_unix_process_id(name)
                    .await
                    .map(|p| p as i32)
                    .unwrap_or(0),
                Err(_) => 0,
            };
            let Ok(app) = app_ref.into_accessible_proxy(self.conn.connection()).await else {
                continue;
            };
            let Ok(windows) = app.get_children().await else {
                continue;
            };
            for win_ref in windows {
                let Ok(win) = win_ref.into_accessible_proxy(self.conn.connection()).await else {
                    continue;
                };
                let mut budget = MAX_NODES;
                if let Ok(tree) = self.walk(&win, 0, &mut budget).await {
                    result.entry(pid).or_default().push(tree);
                }
            }
        }
        Ok(result)
    }

    async fn walk(
        &self,
        node: &AccessibleProxy<'_>,
        depth: usize,
        budget: &mut usize,
    ) -> anyhow::Result<Value> {
        if *budget == 0 {
            anyhow::bail!("node budget exhausted");
        }
        *budget -= 1;

        let role = node.get_role().await.unwrap_or(Role::Invalid);
        let name = node.name().await.unwrap_or_default();
        let mut obj = serde_json::Map::new();
        obj.insert("role".into(), json!(role.name()));
        if !name.is_empty() {
            obj.insert("name".into(), json!(name));
        }
        if let Ok(desc) = node.description().await {
            if !desc.is_empty() {
                obj.insert("description".into(), json!(desc));
            }
        }
        // Window-relative pixel extents, so the model can aim clicks at this
        // element with window-relative coordinates.
        if let Ok(interfaces) = node.get_interfaces().await {
            if interfaces.contains(Interface::Component) {
                if let Ok(proxies) = node.proxies().await {
                    if let Ok(component) = proxies.component().await {
                        if let Ok((x, y, w, h)) = component.get_extents(CoordType::Window).await {
                            if w > 0 && h > 0 {
                                obj.insert("bounds".into(), json!([x, y, w, h]));
                            }
                        }
                    }
                }
            }
        }
        if let Ok(states) = node.get_state().await {
            let interesting: Vec<String> = states
                .iter()
                .filter(|s| {
                    use atspi::State::*;
                    matches!(
                        s,
                        Focused | Checked | Selected | Expanded | Editable | Sensitive | Showing
                    )
                })
                .map(|s| format!("{s:?}"))
                .collect();
            if !interesting.is_empty() {
                obj.insert("states".into(), json!(interesting));
            }
        }

        if depth < MAX_DEPTH && *budget > 0 {
            if let Ok(children) = node.get_children().await {
                let mut child_values = Vec::new();
                for child_ref in children {
                    if *budget == 0 {
                        child_values.push(json!("…truncated"));
                        break;
                    }
                    let Ok(child) = child_ref
                        .into_accessible_proxy(self.conn.connection())
                        .await
                    else {
                        continue;
                    };
                    if let Ok(v) = Box::pin(self.walk(&child, depth + 1, budget)).await {
                        child_values.push(v);
                    }
                }
                if !child_values.is_empty() {
                    obj.insert("children".into(), json!(child_values));
                }
            }
        }
        Ok(Value::Object(obj))
    }
}
