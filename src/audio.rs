// ─────────────────────────────────────────────────────────────────────────────
// PipeShare — Audio Routing Engine
// ─────────────────────────────────────────────────────────────────────────────
//! Creates virtual audio devices via PipeWire-Pulse interface, mixing
//! application audio and real microphone audio into a single virtual microphone.
//!
//! ## Audio Flow
//! ```text
//!   Firefox Audio ──┐
//!                   ├──► [PipeShare_Mix Sink] ──► .monitor ──► Virtual Mic
//!   Real Microphone ┘                                              │
//!                                                                  ▼
//!                                                          Discord/Element
//! ```
//!
//! This ensures the remote party hears both your voice and application audio.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

// ─── Data Structures ───────────────────────────────────────────────────────────

/// Represents an audio node found in PipeWire.
#[derive(Debug, Clone)]
pub struct AudioNode {
    /// PipeWire node id (internal sequence number)
    #[allow(dead_code)]
    pub id: u32,
    /// Application name (e.g., "Firefox", "Spotify")
    pub app_name: String,
}

/// Represents an active audio routing session.
/// Used for cleanup when screen sharing stops.
#[derive(Debug)]
pub struct AudioRoute {
    /// Created PulseAudio module IDs — stored for cleanup
    pub module_ids: Vec<u32>,
    /// Target application names being routed
    pub target_apps: Vec<String>,
    /// Previous default source prior to routing (to restore later)
    pub previous_default_source: Option<String>,
}

// ─── PipeWire Helpers ───────────────────────────────────────────────────────

/// Executes the `pw-link` command.
async fn run_pw_link(args: &[&str]) -> Result<String> {
    let output = Command::new("pw-link")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to execute pw-link command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already linked") {
            debug!("Link already exists, skipping");
            return Ok(String::new());
        }
        anyhow::bail!("pw-link error: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Executes the `pactl` command.
async fn run_pactl(args: &[&str]) -> Result<String> {
    let output = Command::new("pactl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to execute pactl command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("pactl error: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ─── Filters ─────────────────────────────────────────────────────────────────

/// System hardware/internal nodes prefixes to filter out.
const FILTERED_PREFIXES: &[&str] = &[
    "pipewire",
    "PipeWire",
    "pipeshare",
    "PipeShare",
    "alsa_output",
    "alsa_input",
    "Midi-Bridge",
    "bluez_",
    "kwin_",
    "xdg-desktop-portal",
];

// ─── Main Functions ──────────────────────────────────────────────────────────

/// Lists applications currently producing audio on the system.
pub async fn list_audio_sources() -> Result<Vec<AudioNode>> {
    let output = run_pw_link(&["-o"]).await?;
    let mut nodes: HashMap<String, AudioNode> = HashMap::new();
    let mut next_id: u32 = 1;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Only include output and playback ports.
        // Skip microphone inputs or monitor outputs (virtual capturers).
        if line.contains(":monitor_") || line.contains(":capture_") {
            continue;
        }

        if let Some(app_name) = line.split(':').next() {
            let app_name = app_name.trim().to_string();

            if FILTERED_PREFIXES
                .iter()
                .any(|prefix| app_name.starts_with(prefix))
            {
                continue;
            }

            if !nodes.contains_key(&app_name) {
                nodes.insert(
                    app_name.clone(),
                    AudioNode {
                        id: next_id,
                        app_name: app_name.clone(),
                    },
                );
                next_id += 1;
            }
        }
    }

    let result: Vec<AudioNode> = nodes.into_values().collect();
    info!("Found {} audio sources", result.len());
    Ok(result)
}

/// Retrieves the system's current default source (microphone).
async fn get_default_source() -> Result<String> {
    let output = run_pactl(&["info"]).await?;
    for line in output.lines() {
        if line.contains("Default Source:") {
            return Ok(line.split(':').nth(1).unwrap_or("").trim().to_string());
        }
    }
    anyhow::bail!("Default audio source not found")
}

/// Retrieves the system's current default sink (speaker/headphones).
#[allow(dead_code)]
async fn get_default_sink() -> Result<String> {
    let output = run_pactl(&["info"]).await?;
    for line in output.lines() {
        if line.contains("Default Sink:") {
            return Ok(line.split(':').nth(1).unwrap_or("").trim().to_string());
        }
    }
    anyhow::bail!("Default audio sink not found")
}

/// Creates a complete audio route combining selected applications' audio
/// and the real microphone.
///
/// ## Architecture (pw-link Parallel Tap)
/// ```text
///   Firefox Audio ──┬──► [User's Speaker] (unchanged, user-controlled)
///                   │
///                   └──► [PipeShare_Mix] ──► .monitor ──► Virtual Mic
///   Real Microphone ──────┘                                    │
///                                                              ▼
///                                                       Discord/Element
/// ```
///
/// Key difference from the old approach: we DO NOT move the app's audio
/// away from the user's speaker. Instead, we use `pw-link` to create a
/// parallel copy. This means the user can freely switch between GA104 HDMI,
/// HyperX, or any other output device without breaking audio routing.
pub async fn create_audio_route(target_app_names: &[String]) -> Result<AudioRoute> {
    let mut module_ids: Vec<u32> = Vec::new();

    // Save current default source (microphone)
    let prev_source = get_default_source().await.ok();

    info!(
        "[*] Current default microphone: {}",
        prev_source.as_deref().unwrap_or("unknown")
    );

    // ─── Step 1: Create PipeShare_Mix (Null Sink) ───────────────────────
    // This is the mixer: it receives app audio (via pw-link) and mic audio
    // (via loopback). Its monitor becomes the virtual microphone.
    let sink_id = run_pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=PipeShare_Mix",
        r#"sink_properties="device.description=PipeShare_Mix device.class=filter""#,
        "channel_map=stereo",
    ])
    .await?;
    if let Ok(mid) = sink_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }
    info!("[+] PipeShare_Mix created");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ─── Step 2: Create Virtual Microphone (Remap Source) ───────────────
    let source_id = run_pactl(&[
        "load-module",
        "module-remap-source",
        "source_name=PipeShare_Mic",
        "master=PipeShare_Mix.monitor",
        r#"source_properties="device.description=PipeShare_Mic device.class=filter""#,
    ])
    .await?;
    if let Ok(mid) = source_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }
    info!("[+] PipeShare_Mic virtual microphone created");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ─── Step 3: Real microphone → PipeShare_Mix ────────────────────────
    // Remote party hears your voice.
    if let Some(ref mic) = prev_source {
        if let Ok(id) = run_pactl(&[
            "load-module",
            "module-loopback",
            &format!("source={}", mic),
            "sink=PipeShare_Mix",
            "latency_msec=30",
        ])
        .await
        {
            if let Ok(mid) = id.trim().parse::<u32>() {
                module_ids.push(mid);
            }
        }
        info!("[+] Microphone loopback to PipeShare_Mix established");
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ─── Step 4: Link app audio to PipeShare_Mix (PARALLEL) ─────────────
    // Uses pw-link to create a parallel tap from the app's output ports
    // to PipeShare_Mix input ports. The app's audio KEEPS flowing to
    // whatever speaker the user has selected — we only add a copy.
    for app_name in target_app_names {
        link_app_to_mix(app_name).await?;
    }

    // ─── Step 5: Route recording streams to PipeShare_Mic ───────────────
    // Moves active recording (source-output) streams to our virtual mic.
    move_recording_apps_to_mic("PipeShare_Mic.monitor").await?;

    info!("[+] Audio routing initialized — output device switching is fully supported!");

    Ok(AudioRoute {
        module_ids,
        target_apps: target_app_names.to_vec(),
        previous_default_source: prev_source,
    })
}

/// Public wrapper to re-create pw-link connections for an app.
/// Used when screen sharing is toggled (stopped and restarted quickly).
pub async fn relink_app_to_mix(target_app_name: &str) -> Result<()> {
    link_app_to_mix(target_app_name).await
}

/// Creates parallel audio links from an application to PipeShare_Mix via `pw-link`.
///
/// This is **non-destructive**: the app keeps its original audio output.
/// Audio flows to BOTH the user's chosen speaker AND PipeShare_Mix for remote sharing.
///
/// When the user switches output devices (e.g. HyperX → GA104 HDMI),
/// only the original connection changes. The parallel link to PipeShare_Mix persists.
async fn link_app_to_mix(target_app_name: &str) -> Result<()> {
    // Get all output ports in the PipeWire graph
    let output_ports = run_pw_link(&["-o"]).await?;

    // Find app's output ports (e.g., "Firefox:output_FL", "Firefox:output_FR")
    let app_ports: Vec<&str> = output_ports
        .lines()
        .map(|l| l.trim())
        .filter(|l| {
            if let Some(prefix) = l.split(':').next() {
                let prefix = prefix.trim();
                prefix == target_app_name || prefix.starts_with(target_app_name)
            } else {
                false
            }
        })
        // Only output/playback ports — skip monitor and capture ports
        .filter(|l| !l.contains(":monitor_") && !l.contains(":capture_"))
        .collect();

    if app_ports.is_empty() {
        warn!(
            "[-] No audio output ports found for '{}' — app may not be producing audio yet",
            target_app_name
        );
        return Ok(());
    }

    info!("[*] Found {} output ports for '{}'", app_ports.len(), target_app_name);

    // Link each app output port to the corresponding PipeShare_Mix input port
    for port in &app_ports {
        // Determine the channel (FL/FR) based on port name patterns
        let mix_target = if port.contains("FL")
            || port.contains(":output_0")
            || port.contains("front-left")
        {
            "PipeShare_Mix:playback_FL"
        } else if port.contains("FR")
            || port.contains(":output_1")
            || port.contains("front-right")
        {
            "PipeShare_Mix:playback_FR"
        } else {
            // Mono or unknown channel — route to FL
            "PipeShare_Mix:playback_FL"
        };

        match run_pw_link(&[port, mix_target]).await {
            Ok(_) => info!("[+] {} → {} (parallel link)", port, mix_target),
            Err(e) => warn!("[-] Failed to link {}: {}", port, e),
        }
    }

    info!("[+] {} audio tapped successfully — output device remains user-controlled", target_app_name);
    Ok(())
}

/// Moves active recording streams (source-outputs) to our virtual microphone.
/// This targets WebRTC applications (Element, Discord, Firefox, Chrome) and routes
/// them silently to `PipeShare_Mic.monitor` without touching the global default microphone.
async fn move_recording_apps_to_mic(virtual_mic_name: &str) -> Result<()> {
    let output = run_pactl(&["list", "source-outputs"]).await?;
    let mut current_id = None;

    for line in output.lines() {
        let trimmed = line.trim();
        
        // Find the recording stream ID
        if trimmed.starts_with("Source Output #") {
            current_id = trimmed.strip_prefix("Source Output #").map(|s| s.to_string());
        } 
        // Identify the application name
        else if trimmed.starts_with("application.name =") {
            if let Some(name_str) = trimmed.split('=').nth(1) {
                let name = name_str.trim().trim_matches('"');
                
                // We want to move communication apps that are actively recording.
                // We exclude system processes or PipeWire's own loops to prevent feedback loops.
                if !FILTERED_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
                    if let Some(ref id) = current_id {
                        match run_pactl(&["move-source-output", id, virtual_mic_name]).await {
                            Ok(_) => info!("[+] Routed recording app '{}' (ID: {}) to {}", name, id, virtual_mic_name),
                            Err(e) => debug!("[-] Skipping move for recording app {}: {}", name, e),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Helper to create a route for a single application.
pub async fn create_audio_route_single(target_app_name: &str) -> Result<AudioRoute> {
    create_audio_route(&[target_app_name.to_string()]).await
}

/// Fully cleans up an active audio routing session.
///
/// 1. Restores recording streams to the default mic FIRST
/// 2. Unloads all PipeShare PulseAudio modules
/// 3. pw-link connections auto-disconnect when the null-sink is destroyed
pub async fn destroy_audio_route(route: &AudioRoute) -> Result<()> {
    info!("[*] Cleaning up audio route for: {:?}", route.target_apps);

    // CRITICAL: Restore recording streams BEFORE destroying modules.
    // If we destroy PipeShare_Mic first, Element's WebRTC source-output
    // would point to a non-existent source, breaking its entire pipeline.
    // This is why the share button stops working after the first share.
    restore_recording_streams_to_default().await;

    // Unload modules in reverse order (dependency order)
    for module_id in route.module_ids.iter().rev() {
        match run_pactl(&["unload-module", &module_id.to_string()]).await {
            Ok(_) => info!("[-] Module {} unloaded", module_id),
            Err(e) => error!("[-] Failed to unload module {}: {}", module_id, e),
        }
    }

    info!("[+] Cleanup complete — system returned to normal state");
    Ok(())
}

/// Moves all recording streams currently using PipeShare sources back to
/// the system's default microphone. Must be called BEFORE unloading modules.
async fn restore_recording_streams_to_default() {
    let default_source = match get_default_source().await {
        Ok(s) => s,
        Err(_) => {
            warn!("[-] Could not determine default source for restoration");
            return;
        }
    };

    let output = match run_pactl(&["list", "source-outputs"]).await {
        Ok(o) => o,
        Err(_) => return,
    };

    let mut current_id: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Source Output #") {
            current_id = trimmed.strip_prefix("Source Output #").map(|s| s.to_string());
        } else if trimmed.contains("PipeShare") || trimmed.contains("pipeshare") {
            // This source-output is connected to a PipeShare source — restore it
            if let Some(ref id) = current_id {
                match run_pactl(&["move-source-output", id, &default_source]).await {
                    Ok(_) => info!("[+] Restored recording stream {} to {}", id, default_source),
                    Err(e) => debug!("[-] Could not restore stream {}: {}", id, e),
                }
            }
        }
    }
}

/// Discovers and unloads all leftover PipeShare modules across the system.
/// Intended for the `pipeshare stop` command.
pub async fn cleanup_all() -> Result<u32> {
    let output = Command::new("pactl")
        .args(["list", "modules", "short"])
        .stdout(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut cleaned = 0;

    for line in stdout.lines() {
        if line.contains("PipeShare") || line.contains("pipeshare") {
            if let Some(id_str) = line.split_whitespace().next() {
                let _ = Command::new("pactl")
                    .args(["unload-module", id_str])
                    .output()
                    .await;
                info!("[-] Module {} unloaded", id_str);
                cleaned += 1;
            }
        }
    }

    // Restore default source
    // (Find real ALSA source and set it)
    let sources = Command::new("pactl")
        .args(["list", "sources", "short"])
        .stdout(Stdio::piped())
        .output()
        .await?;
    let src_stdout = String::from_utf8_lossy(&sources.stdout);
    for line in src_stdout.lines() {
        if line.contains("alsa_input") && !line.contains("PipeShare") {
            if let Some(name) = line.split_whitespace().nth(1) {
                let _ = run_pactl(&["set-default-source", name]).await;
                info!("[+] Default microphone restored: {}", name);
                break;
            }
        }
    }

    Ok(cleaned)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_list_audio_sources() {
        let result = list_audio_sources().await;
        assert!(result.is_ok(), "Failed to list audio sources");
        println!("Found sources: {:?}", result.unwrap());
    }

    #[tokio::test]
    async fn test_get_default_source() {
        let result = get_default_source().await;
        assert!(result.is_ok());
        println!("Default source: {}", result.unwrap());
    }
}
