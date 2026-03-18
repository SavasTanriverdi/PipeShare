// ─────────────────────────────────────────────────────────────────────────────
// PipeShare — Screen Share Detection Engine
// ─────────────────────────────────────────────────────────────────────────────
//! Detects screen sharing events in REAL-TIME and EVENT-DRIVEN manner.
//!
//! ## Architecture Note
//!
//! Wayland's security model prevents 3rd-party applications from accessing
//! "who is sharing the screen" information via D-Bus. Portal signals
//! flow only between the requesting application and the portal.
//!
//! Therefore, PipeShare uses a two-tiered detection strategy:
//!
//! 1. **PipeWire Graph Monitor** (`pw-dump --monitor`):
//!    PipeWire creates new video stream nodes when screen sharing begins.
//!    `pw-dump --monitor` reports these changes as JSON INSTANTLY
//!    (event-driven, no polling!). This robustly detects the presence
//!    of a genuine ScreenCast session.
//!
//! 2. **D-Bus Portal Status Check**:
//!    Verifies portal existence and accessibility.
//!    However, due to the third-party monitoring restriction, direct
//!    monitoring of portal signals is not possible.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use zbus::Connection;

// ─── Data Structures ───────────────────────────────────────────────────────────

/// Represents events related to screen sharing.
#[derive(Debug, Clone)]
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

// ─── Constants ───────────────────────────────────────────────────────────────

/// Video nodes that are always present and UNRELATED to screen sharing.
/// We must filter these out to prevent false positives.
const ALWAYS_PRESENT_NODES: &[&str] = &[
    "kwin_wayland",
    "plasmashell",
    "xdg-desktop-portal-kde",
    "xdg-desktop-portal",
];

/// media.class values used to identify ScreenCast session nodes.
const SCREENCAST_MEDIA_CLASSES: &[&str] = &[
    "Stream/Input/Video",  // ScreenCast consumer side
    "Stream/Output/Video", // ScreenCast source side
];

// ─── Main Monitoring Function (Event-Driven) ─────────────────────────────────

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
    info!("[*] Starting event-based PipeWire monitor...");

    // Understand the current state — is there an active ScreenCast session already?
    let baseline_ids = get_current_screencast_nodes().await?;
    if !baseline_ids.is_empty() {
        info!(
            "[*] {} initial ScreenCast nodes exist (likely system nodes)",
            baseline_ids.len()
        );
    }

    // This set tracks known screen sharing nodes.
    // Initial nodes are saved as "baseline" to distinguish newly added ones.
    let mut known_screencast_ids: HashSet<u32> = baseline_ids;

    loop {
        info!("[*] Launching pw-dump --monitor...");

        let result = run_monitor_loop(&tx, &mut known_screencast_ids).await;

        match result {
            Ok(_) => {
                warn!("[-] pw-dump --monitor exited unexpectedly, restarting...");
            }
            Err(e) => {
                error!("[-] PipeWire monitor error: {}", e);
                error!("    Retrying in 5 seconds...");
            }
        }

        // Automatic reconnection — self-healing if daemon crashes
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Spawns the `pw-dump --monitor` process and parses its output.
async fn run_monitor_loop(
    tx: &mpsc::Sender<ScreenShareEvent>,
    known_ids: &mut HashSet<u32>,
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

    info!("[+] PipeWire event flow started — waiting for screen sharing...");

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // pw-dump closed
            break;
        }

        let trimmed = line.trim();

        // Track JSON array start/end
        if trimmed == "[" {
            in_array = true;
            json_buffer.clear();
            json_buffer.push_str(trimmed);
            bracket_depth = 1;
            continue;
        }

        if in_array {
            json_buffer.push('\n');
            json_buffer.push_str(trimmed);

            // Track bracket depth
            for ch in trimmed.chars() {
                match ch {
                    '[' => bracket_depth += 1,
                    ']' => bracket_depth -= 1,
                    _ => {}
                }
            }

            // Array closed — parse time
            if bracket_depth == 0 {
                in_array = false;
                process_pw_dump_update(&json_buffer, tx, known_ids).await;
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
async fn process_pw_dump_update(
    json_str: &str,
    tx: &mpsc::Sender<ScreenShareEvent>,
    known_ids: &mut HashSet<u32>,
) {
    let objects: Vec<PwDumpObject> = match serde_json::from_str(json_str) {
        Ok(objs) => objs,
        Err(e) => {
            debug!("JSON parsing error (skipping update): {}", e);
            return;
        }
    };

    // Collect all video node IDs in this update
    let mut current_video_ids: HashSet<u32> = HashSet::new();

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

        current_video_ids.insert(obj.id);

        // Is it a new ScreenCast node?
        if !known_ids.contains(&obj.id) {
            let app_name = props
                .application_name
                .clone()
                .or_else(|| props.node_name.clone());

            info!(
                "[+] NEW screen sharing detected! node_id={}, app={}, media.class={}",
                obj.id,
                app_name.as_deref().unwrap_or("unknown"),
                media_class
            );

            known_ids.insert(obj.id);

            let _ = tx
                .send(ScreenShareEvent::Started {
                    app_name,
                    node_id: obj.id,
                })
                .await;
        }
    }

    // Detect closed ScreenCast nodes
    let removed: Vec<u32> = known_ids
        .iter()
        .filter(|id| !current_video_ids.contains(id))
        .copied()
        .collect();

    for node_id in removed {
        // However, only remove those within the scope of this update
        // (pw-dump --monitor might not send the whole graph every time)
        // Therefore, we solely declare a node stopped if the node's absence
        // is certain according to `current_video_ids`.
        if !current_video_ids.is_empty() {
            info!("[-] Screen sharing STOPPED! node_id={}", node_id);
            known_ids.remove(&node_id);
            let _ = tx.send(ScreenShareEvent::Stopped { node_id }).await;
        }
    }
}

/// Returns the IDs of ScreenCast nodes in the current PipeWire graph.
///
/// Used initially to establish a "baseline" — so that we can isolate
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

// ─── D-Bus Portal Checks ─────────────────────────────────────────────────────

/// Queries the `org.freedesktop.portal.ScreenCast` portal via D-Bus.
///
/// Ensures the portal service is accessible. Used to verify
/// the system has working portal support.
pub async fn check_portal_available() -> Result<bool> {
    let connection = Connection::session()
        .await
        .context("Failed to establish D-Bus session connection")?;

    // Check presence of portal service
    let reply = connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &"org.freedesktop.portal.Desktop",
        )
        .await;

    match reply {
        Ok(msg) => {
            let has_owner: bool = msg.body().deserialize().unwrap_or(false);
            if has_owner {
                info!("[+] XDG Desktop Portal is active and registered");
            } else {
                warn!("[-] XDG Desktop Portal service not found");
            }
            Ok(has_owner)
        }
        Err(e) => {
            error!("[-] Portal check error: {}", e);
            Ok(false)
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_portal_check() {
        let result = check_portal_available().await;
        assert!(result.is_ok());
        println!("Portal status: {:?}", result.unwrap());
    }

    #[tokio::test]
    async fn test_baseline_screencast_nodes() {
        let nodes = get_current_screencast_nodes().await;
        assert!(nodes.is_ok());
        println!("Baseline ScreenCast nodes: {:?}", nodes.unwrap());
    }
}
