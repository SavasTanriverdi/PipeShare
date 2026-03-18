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

    // Main event loop
    while let Some(event) = rx.recv().await {
        match event {
            ScreenShareEvent::Started { app_name, node_id } => {
                let detected_name = app_name.unwrap_or_else(|| "Unknown".to_string());
                info!(
                    "[+] Screen share detected via app={}, node={}",
                    detected_name, node_id
                );

                // If already active, disregard duplicate events
                if active_route.is_some() {
                    info!("[*] Audio routing already initialized, skipping request");
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

                // DEBOUNCE: When user toggles screen sharing (stop → restart),
                // PipeWire sends Stopped and Started events nearly simultaneously.
                // Wait briefly to see if a new session immediately replaces this one.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                // Drain any pending Started events that arrived during the wait
                let mut new_share_active = false;
                while let Ok(pending) = rx.try_recv() {
                    match pending {
                        ScreenShareEvent::Started { app_name, node_id } => {
                            info!(
                                "[+] New screen share detected during debounce (node: {})",
                                node_id
                            );
                            new_share_active = true;
                            // Re-link audio for the existing route if we have one
                            if let Some(ref route) = active_route {
                                info!("[*] Re-linking audio for existing route...");
                                for app_name in &route.target_apps {
                                    let _ = audio::relink_app_to_mix(app_name).await;
                                }
                            }
                        }
                        ScreenShareEvent::Stopped { node_id: other_id } => {
                            debug!(
                                "[-] Additional stop event during debounce (node: {})",
                                other_id
                            );
                        }
                    }
                }

                if new_share_active {
                    info!("[*] Screen share was restarted — keeping audio route alive");
                    continue;
                }

                // No new share started — clean up
                if let Some(route) = active_route.take() {
                    if let Err(e) = audio::destroy_audio_route(&route).await {
                        error!("[-] Cleanup failure: {}", e);
                    } else {
                        send_notification(
                            "Audio Share Terminated",
                            "Screen sharing has concluded. Audio routing is now disabled.",
                        )
                        .await;
                    }
                }
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
