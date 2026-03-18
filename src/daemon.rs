// ─────────────────────────────────────────────────────────────────────────────
// PipeShare — Daemon (Background Service)
// ─────────────────────────────────────────────────────────────────────────────
//! The core engine of PipeShare. Runs silently in the background:
//! 1. Monitors the PipeWire graph event-based (no polling)
//! 2. Prompts the user when a screen share is detected
//! 3. Automatically routes selected application audio + mic
//! 4. Unloads configurations when screen share ceases
//!
//! ## Lifecycle
//! ```text
//!   [Start] -> [Monitor] -> [Detect] -> [Prompt] -> [Route] -> [Cleanup] -> [Monitor]
//! ```

use anyhow::Result;
use std::process::Stdio;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::audio;
use crate::dbus_monitor::{self, ScreenShareEvent};

/// The main loop of the daemon — runs as a background service.
///
/// This function never returns voluntarily. It stops on Ctrl+C or
/// a system signal.
pub async fn run_daemon() -> Result<()> {
    info!("[*] Starting PipeShare daemon...");

    // Check system dependencies first
    let portal_ok = dbus_monitor::check_portal_available().await?;
    if !portal_ok {
        error!("[-] XDG Desktop Portal not found! Daemon cannot operate.");
        anyhow::bail!("XDG Desktop Portal is required");
    }

    // Create the event channel
    let (tx, mut rx) = mpsc::channel::<ScreenShareEvent>(32);

    // Spawn PipeWire monitor component
    tokio::spawn(async move {
        if let Err(e) = dbus_monitor::monitor_screen_share(tx).await {
            error!("[-] PipeWire monitor error: {}", e);
        }
    });

    info!("[+] Daemon ready — standing by for screen share sequences...");

    // Track active audio routing
    let mut active_route: Option<audio::AudioRoute> = None;
    let mut active_node_id: Option<u32> = None;
    let mut pending_cleanup = false;

    // Main event loop
    loop {
        let event_opt = if pending_cleanup {
            match tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv()).await {
                Ok(Some(e)) => Some(e),
                Ok(None) => None,
                Err(_) => {
                    info!("[*] Cleanup timeout reached. Destroying audio route.");
                    if let Some(route) = active_route.take() {
                        let _ = audio::destroy_audio_route(&route).await;
                        // Removed the annoying "Audio Share Terminated" notification as requested
                    }
                    active_node_id = None;
                    pending_cleanup = false;
                    continue;
                }
            }
        } else {
            rx.recv().await
        };

        let Some(event) = event_opt else {
            break;
        };

        match event {
            ScreenShareEvent::Started { app_name, node_id } => {
                if pending_cleanup {
                    info!("[*] Reconnect detected within 3s timeout! Cancelling cleanup.");
                    pending_cleanup = false;
                    if let Some(ref route) = active_route {
                        for app in &route.target_apps {
                            let _ = audio::relink_app_to_mix(app).await;
                        }
                    }
                    active_node_id = Some(node_id);
                    continue;
                }

                let detected_name = app_name.unwrap_or_else(|| "Unknown".to_string());
                info!(
                    "[+] Screen share detected via app={}, node={}",
                    detected_name, node_id
                );

                // Update the active node ID — even if route exists
                let old_node = active_node_id.replace(node_id);
                if old_node.is_some() {
                    info!(
                        "[*] Screen share node replaced: {:?} → {}",
                        old_node, node_id
                    );
                }

                // If route already exists, just re-link apps (node changed but route is same)
                if active_route.is_some() {
                    info!("[*] Audio route already active, re-linking apps for new node");
                    if let Some(ref route) = active_route {
                        for app in &route.target_apps {
                            let _ = audio::relink_app_to_mix(app).await;
                        }
                    }
                    continue;
                }

                // Ask the user which application audio to share
                match ask_user_for_audio_source().await {
                    Ok(Some(selected_apps)) => {
                        info!("[*] User selection: {:?}", selected_apps);
                        match audio::create_audio_route(&selected_apps).await {
                            Ok(route) => {
                                send_notification(
                                    "Audio Routing Enabled",
                                    &format!(
                                        "Routing audio for {}. It will be cleaned up automatically once sharing concludes.",
                                        selected_apps.join(", ")
                                    ),
                                )
                                .await;
                                active_route = Some(route);
                            }
                            Err(e) => {
                                error!("[-] Route creation failed: {}", e);
                                send_notification(
                                    "Error",
                                    &format!("Route creation failed: {}", e),
                                )
                                .await;
                            }
                        }
                    }
                    Ok(None) => {
                        info!("[-] User rejected/cancelled audio sharing request");
                    }
                    Err(e) => {
                        warn!("[-] Dialog display failure: {}", e);
                    }
                }
            }
            ScreenShareEvent::Stopped { node_id } => {
                info!("[-] Screen Share ceased (node: {})", node_id);

                // Only clean up if the CURRENT active node stopped.
                // If an OLD node stopped (replaced by a new one), ignore it.
                if active_node_id != Some(node_id) {
                    info!(
                        "[*] Ignoring stop for old node {} (current: {:?})",
                        node_id, active_node_id
                    );
                    continue;
                }

                info!(
                    "[*] Screen Share stopped. Waiting 3s for possible WebRTC renegotiation reconnects..."
                );
                pending_cleanup = true;
            }
        }
    }

    Ok(())
}

// ─── Dialog and Notification Entities ────────────────────────────────────────

/// Posts desktop notification via `notify-send`.
async fn send_notification(title: &str, body: &str) {
    // Attempt notify-send command
    let result = tokio::process::Command::new("notify-send")
        .args([
            "--app-name=PipeShare",
            "--icon=audio-card",
            "--urgency=normal",
            title,
            body,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    if let Err(e) = result {
        warn!("[-] Failed to execute notify-send: {}", e);
    }
}

/// Prompts the user with a dialog to pick applications to broadcast audio for.
/// (Relies on KDE dialog utility `kdialog` mapped via Zenity, if the latter is present)
///
/// Returns:
/// - `Ok(Some(vec))` — Apps were selected
/// - `Ok(None)` — Denied or aborted
/// - `Err(_)` — Fallback / Display error
async fn ask_user_for_audio_source() -> Result<Option<Vec<String>>> {
    // Collect available system audio sources
    let sources = audio::list_audio_sources().await?;

    if sources.is_empty() {
        send_notification(
            "PipeShare",
            "No audio-producing applications were located. Start video playback in a browser first.",
        )
        .await;
        return Ok(None);
    }

    // Setup the kdialog arguments
    let mut args: Vec<String> = vec![
        "--checklist".to_string(),
        "Screen share detected!\nPlease specify the application audio sources to share."
            .to_string(),
    ];

    for source in &sources {
        args.push(source.app_name.clone()); // tag
        args.push(source.app_name.clone()); // label
        args.push("on".to_string()); // default: all enabled
    }

    args.push("--title".to_string());
    args.push("PipeShare — Audio Link".to_string());

    let output = tokio::process::Command::new("kdialog")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(out) => {
            if !out.status.success() {
                // Return no-app upon user abort
                return Ok(None);
            }
            let selected = String::from_utf8_lossy(&out.stdout);

            // kdialog `--checklist` output format: "App 1" "App 2"
            // Use quotes tracking instead of whitespace split as app titles may contain spaces
            let mut selected_apps = Vec::new();
            let mut current_app = String::new();
            let mut in_quotes = false;

            for c in selected.chars() {
                if c == '"' {
                    if in_quotes {
                        if !current_app.trim().is_empty() {
                            selected_apps.push(current_app.clone());
                        }
                        current_app.clear();
                        in_quotes = false;
                    } else {
                        in_quotes = true;
                    }
                } else if in_quotes {
                    current_app.push(c);
                }
            }

            // Fallback: simple text output instead of quotes
            if selected_apps.is_empty() && !selected.trim().is_empty() {
                selected_apps = selected
                    .trim()
                    .split('\n')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }

            if selected_apps.is_empty() {
                return Ok(None);
            }

            Ok(Some(selected_apps))
        }
        Err(_) => {
            // Resort to Zenity dialog integration on instances missing `kdialog`
            try_zenity_dialog(&sources).await
        }
    }
}

/// Zenity-Based dialog UI trigger component.
async fn try_zenity_dialog(sources: &[audio::AudioNode]) -> Result<Option<Vec<String>>> {
    let mut args: Vec<String> = vec![
        "--list".to_string(),
        "--checklist".to_string(),
        "--title=PipeShare — Audio Link".to_string(),
        "--text=Please select applications to capture:".to_string(),
        "--column=".to_string(),
        "--column=Application".to_string(),
        "--separator= ".to_string(),
    ];

    for source in sources {
        args.push("TRUE".to_string());
        args.push(source.app_name.clone());
    }

    let output = tokio::process::Command::new("zenity")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        return Ok(None);
    }

    let selected = String::from_utf8_lossy(&output.stdout);
    let selected_apps: Vec<String> = selected
        .trim()
        .split_whitespace()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if selected_apps.is_empty() {
        return Ok(None);
    }

    Ok(Some(selected_apps))
}
