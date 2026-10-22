//! Screen share detection using PipeWire graph monitoring and D-Bus portal status.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zbus::Connection;

/// Represents events related to screen sharing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenShareEvent {
    /// A new screen sharing session has started
    Started {
        /// Name of the initiating application (if detectable)
        app_name: Option<String>,
        /// PipeWire node ID
        node_id: u32,
    },
    /// A screen sharing session has ended
    Stopped {
        /// ID of the closed node
        node_id: u32,
    },
}

/// Represents a single PipeWire object in `pw-dump` JSON output.
/// We only deserialize the fields we need.
#[derive(Debug, Deserialize)]
struct PwDumpObject {
    id: u32,
    #[serde(rename = "type")]
    _obj_type: Option<String>,
    info: Option<PwDumpInfo>,
}

#[derive(Debug, Deserialize)]
struct PwDumpInfo {
    props: Option<PwDumpProps>,
}

#[derive(Debug, Deserialize)]
struct PwDumpProps {
    #[serde(rename = "media.class")]
    media_class: Option<String>,
    #[serde(rename = "node.name")]
    node_name: Option<String>,
    #[serde(rename = "application.name")]
    application_name: Option<String>,
    #[serde(rename = "media.role")]
    _media_role: Option<String>,
}

/// Video nodes that are always present and UNRELATED to screen sharing.
/// We must filter these out to prevent false positives across Wayland compositors.
const ALWAYS_PRESENT_NODES: &[&str] = &[
    "kwin_wayland",
    "plasmashell",
    "mutter",
    "gnome-shell",
    "xdg-desktop-portal-kde",
    "xdg-desktop-portal-gnome",
    "xdg-desktop-portal-hyprland",
    "xdg-desktop-portal-wlr",
    "xdg-desktop-portal",
];

/// Supported XDG Desktop Portal ScreenCast backends across Wayland compositors.
const PORTAL_BACKENDS: &[&str] = &[
    "org.freedesktop.impl.portal.desktop.kde",
    "org.freedesktop.impl.portal.desktop.gnome",
    "org.freedesktop.impl.portal.desktop.hyprland",
    "org.freedesktop.impl.portal.desktop.wlr",
    "org.freedesktop.impl.portal.desktop.gtk",
];

/// media.class values used to identify ScreenCast session nodes.
const SCREENCAST_MEDIA_CLASSES: &[&str] = &[
    "Stream/Input/Video",  // ScreenCast consumer side
    "Stream/Output/Video", // ScreenCast source side
];

/// Monitors the PipeWire graph in REAL-TIME using `pw-dump --monitor`.
///
/// This function **does not poll**. The `pw-dump --monitor` command connects to
/// the PipeWire daemon and outputs JSON whenever the graph changes (node added/removed).
/// We parse this output line by line to detect ScreenCast nodes.
///
/// # Performance
/// - CPU Usage: ~0% (sleeps while waiting for events, wakes only on changes)
/// - RAM: ~1 MB (json parse buffer)
pub async fn monitor_screen_share(tx: mpsc::Sender<ScreenShareEvent>) -> Result<()> {
    info!("Starting event-based PipeWire monitor...");

    // Understand the current state -- is there an active ScreenCast session already?
    let baseline_ids = get_current_screencast_nodes().await?;
    if !baseline_ids.is_empty() {
        info!(
            "{} initial ScreenCast nodes exist (likely system nodes)",
            baseline_ids.len()
        );
    }

    // This set tracks known screen sharing nodes.
    // Initial nodes are saved as "baseline" to distinguish newly added ones.
    let mut known_screencast_ids: HashSet<u32> = baseline_ids;
    // Tracks ONLY nodes detected via Started events (not baseline)
    let mut active_session_ids: HashSet<u32> = HashSet::new();

    loop {
        info!("Launching pw-dump --monitor...");

        let result =
            run_monitor_loop(&tx, &mut known_screencast_ids, &mut active_session_ids).await;

        match result {
            Ok(_) => {
                warn!("pw-dump --monitor exited unexpectedly, restarting...");
            }
            Err(e) => {
                error!("PipeWire monitor error: {}", e);
                error!("    Retrying in 5 seconds...");
            }
        }

        // Automatic reconnection -- self-healing if daemon crashes
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Updates JSON array bracket depth while ignoring brackets inside string literals.
fn update_bracket_depth(line: &str, mut depth: i32) -> i32 {
    let mut in_string = false;
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if !in_string {
            match ch {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
        }
    }

    depth
}

/// Spawns the `pw-dump --monitor` process and parses its output.
async fn run_monitor_loop(
    tx: &mpsc::Sender<ScreenShareEvent>,
    known_ids: &mut HashSet<u32>,
    active_session_ids: &mut HashSet<u32>,
) -> Result<()> {
    let mut child = tokio::process::Command::new("pw-dump")
        .args(["--monitor", "--no-colors"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn pw-dump command. Is PipeWire installed?")?;

    let stdout = child
        .stdout
        .take()
        .context("Failed to capture pw-dump stdout")?;

    let mut reader = BufReader::new(stdout);
    let mut json_buffer = String::new();
    let mut bracket_depth: i32 = 0;
    let mut in_array = false;

    info!("PipeWire event flow started -- waiting for screen sharing...");

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // pw-dump closed
            break;
        }

        let trimmed = line.trim();

        // Track JSON array start/end
        if !in_array && trimmed == "[" {
            in_array = true;
            json_buffer.clear();
            json_buffer.push_str(trimmed);
            bracket_depth = 1;
            continue;
        }

        if in_array {
            json_buffer.push('\n');
            json_buffer.push_str(trimmed);

            bracket_depth = update_bracket_depth(trimmed, bracket_depth);

            // Array closed -- parse time
            if bracket_depth <= 0 {
                in_array = false;
                process_pw_dump_update(&json_buffer, tx, known_ids, active_session_ids).await;
                json_buffer.clear();
            }
        }
    }

    // child process cleanup
    let _ = child.kill().await;
    Ok(())
}

/// Processes a single `pw-dump` JSON update.
///
/// Detects new ScreenCast nodes and identifies vanished nodes.
///
/// `known_ids` contains ALL known screencast node IDs (baseline + active).
/// `active_session_ids` contains ONLY nodes detected via Started events.
/// Stopped events are only sent for nodes in `active_session_ids`.
async fn process_pw_dump_update(
    json_str: &str,
    tx: &mpsc::Sender<ScreenShareEvent>,
    known_ids: &mut HashSet<u32>,
    active_session_ids: &mut HashSet<u32>,
) {
    // pw-dump can output mixed-type JSON arrays (objects + strings like "Audio/Sink").
    // Parse as Value first, then filter to only objects.
    let raw_values: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
        Ok(vals) => vals,
        Err(e) => {
            debug!("JSON parsing error (skipping update): {}", e);
            return;
        }
    };

    // Filter to only object values and deserialize them
    let objects: Vec<PwDumpObject> = raw_values
        .into_iter()
        .filter(|v| v.is_object())
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    if objects.is_empty() {
        return;
    }

    // 1. Process explicit deletions first
    // When PipeWire destroys a node, pw-dump sends the id with info: null
    let mut removed_ids = Vec::new();
    for obj in &objects {
        if obj.info.is_none() && active_session_ids.contains(&obj.id) {
            removed_ids.push(obj.id);
        }
    }

    for node_id in removed_ids {
        info!("Screen sharing STOPPED! node_id={}", node_id);
        known_ids.remove(&node_id);
        active_session_ids.remove(&node_id);
        let _ = tx.send(ScreenShareEvent::Stopped { node_id }).await;
    }

    // 2. Process additions / updates
    for obj in &objects {
        let Some(info) = &obj.info else { continue };
        let Some(props) = &info.props else { continue };
        let Some(media_class) = &props.media_class else {
            continue;
        };

        // Is this a video stream node?
        if !SCREENCAST_MEDIA_CLASSES.iter().any(|mc| media_class == *mc) {
            continue;
        }

        let node_name = props.node_name.as_deref().unwrap_or("");

        // Skip system nodes that are always present
        if ALWAYS_PRESENT_NODES.iter().any(|n| node_name.contains(n)) {
            continue;
        }

        // Is it a new ScreenCast node?
        if !known_ids.contains(&obj.id) {
            let app_name = props
                .application_name
                .clone()
                .or_else(|| props.node_name.clone());

            info!(
                "NEW screen sharing detected! node_id={}, app={}, media.class={}",
                obj.id,
                app_name.as_deref().unwrap_or("unknown"),
                media_class
            );

            known_ids.insert(obj.id);
            active_session_ids.insert(obj.id);

            let _ = tx
                .send(ScreenShareEvent::Started {
                    app_name,
                    node_id: obj.id,
                })
                .await;
        }
    }
}

/// Returns the IDs of ScreenCast nodes in the current PipeWire graph.
///
/// Used initially to establish a "baseline" -- so that we can isolate
/// new nodes from the already present ones.
async fn get_current_screencast_nodes() -> Result<HashSet<u32>> {
    let output = tokio::process::Command::new("pw-dump")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to run pw-dump")?;

    let objects: Vec<PwDumpObject> = serde_json::from_slice(&output.stdout).unwrap_or_default();

    let mut ids = HashSet::new();

    for obj in &objects {
        let Some(info) = &obj.info else { continue };
        let Some(props) = &info.props else { continue };
        let Some(media_class) = &props.media_class else {
            continue;
        };

        if SCREENCAST_MEDIA_CLASSES.iter().any(|mc| media_class == *mc) {
            let node_name = props.node_name.as_deref().unwrap_or("");
            if !ALWAYS_PRESENT_NODES.iter().any(|n| node_name.contains(n)) {
                ids.insert(obj.id);
            }
        }
    }

    Ok(ids)
}

/// Queries portal availability via D-Bus across supported Wayland desktop backends.
pub async fn check_portal_available() -> Result<bool> {
    let connection = Connection::session()
        .await
        .context("Failed to establish D-Bus session connection")?;

    // Check 1: Main portal service (D-Bus activated, may not be running yet)
    let mut portal_ok = check_dbus_name(&connection, "org.freedesktop.portal.Desktop").await;

    if !portal_ok {
        info!("XDG Desktop Portal not yet on D-Bus, waiting up to 30s...");
        for i in 1..=30 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            portal_ok = check_dbus_name(&connection, "org.freedesktop.portal.Desktop").await;
            if portal_ok {
                info!("XDG Desktop Portal appeared after {}s", i);
                break;
            }
        }
    }

    if !portal_ok {
        warn!("XDG Desktop Portal service not found after 30s");
        return Ok(false);
    }
    info!("XDG Desktop Portal is active and registered");

    // Check 2: Compositor-specific ScreenCast portal backend (KDE, GNOME, Hyprland, wlroots, GTK)
    let mut detected_backend: Option<&str> = None;
    for backend in PORTAL_BACKENDS {
        if check_dbus_name(&connection, backend).await {
            detected_backend = Some(backend);
            break;
        }
    }

    if detected_backend.is_none() {
        info!("Portal backend not yet on D-Bus, waiting up to 15s...");
        for i in 1..=15 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            for backend in PORTAL_BACKENDS {
                if check_dbus_name(&connection, backend).await {
                    detected_backend = Some(backend);
                    info!("Portal backend '{}' appeared after {}s", backend, i);
                    break;
                }
            }
            if detected_backend.is_some() {
                break;
            }
        }
    }

    if let Some(backend) = detected_backend {
        info!("ScreenCast portal backend is available: {}", backend);
    } else {
        warn!("No known ScreenCast portal backend found -- ScreenCast may not work!");
        warn!("    Try: systemctl --user restart xdg-desktop-portal.service");
    }

    Ok(true)
}

/// Helper: checks if a D-Bus bus name is currently owned.
async fn check_dbus_name(connection: &Connection, bus_name: &str) -> bool {
    let reply = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &bus_name,
        )
        .await;

    match reply {
        Ok(msg) => msg.body().deserialize().unwrap_or(false),
        Err(e) => {
            debug!("D-Bus check for '{}' failed: {}", bus_name, e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bracket_depth_ignores_brackets_in_strings() {
        let depth = 1;
        let line = r#"{"application.name": "Firefox [WebRTC]", "nested": [1, 2]}"#;
        assert_eq!(update_bracket_depth(line, depth), 1);
        assert_eq!(update_bracket_depth("]", 1), 0);
    }

    #[tokio::test]
    async fn test_process_pw_dump_lifecycle() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut known_ids = HashSet::new();
        let mut active_ids = HashSet::new();

        // 1. Always-present system node should be ignored
        let system_json = r#"[
            {
                "id": 42,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "media.class": "Stream/Output/Video",
                        "node.name": "kwin_wayland"
                    }
                }
            }
        ]"#;
        process_pw_dump_update(system_json, &tx, &mut known_ids, &mut active_ids).await;
        assert!(rx.try_recv().is_err());

        // 2. Active WebRTC ScreenCast node should trigger Started
        let start_json = r#"[
            {
                "id": 99,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "props": {
                        "media.class": "Stream/Input/Video",
                        "node.name": "webrtc-consumer",
                        "application.name": "Discord"
                    }
                }
            }
        ]"#;
        process_pw_dump_update(start_json, &tx, &mut known_ids, &mut active_ids).await;
        let evt = rx.try_recv().expect("Expected Started event");
        assert_eq!(
            evt,
            ScreenShareEvent::Started {
                app_name: Some("Discord".to_string()),
                node_id: 99,
            }
        );

        // 3. Node deletion (info: null) should trigger Stopped
        let stop_json = r#"[
            {
                "id": 99,
                "info": null
            }
        ]"#;
        process_pw_dump_update(stop_json, &tx, &mut known_ids, &mut active_ids).await;
        let stop_evt = rx.try_recv().expect("Expected Stopped event");
        assert_eq!(stop_evt, ScreenShareEvent::Stopped { node_id: 99 });
    }

    #[tokio::test]
    async fn test_portal_check() {
        let result = check_portal_available().await;
        if result.is_err() {
            eprintln!("Skipping: D-Bus portal not available (CI environment)");
            return;
        }
        println!("Portal check passed");
    }

    #[tokio::test]
    async fn test_baseline_screencast_nodes() {
        let nodes = get_current_screencast_nodes().await;
        if nodes.is_err() {
            eprintln!("Skipping: PipeWire not available (CI environment)");
            return;
        }
        let node_list = nodes.unwrap();
        println!("Found {} baseline ScreenCast nodes", node_list.len());
    }
}
