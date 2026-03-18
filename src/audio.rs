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
/// ## Created Structure
/// 1. **PipeShare_AppSink** (null-sink): Intermediate sink for app audio
/// 2. **PipeShare_Mix** (null-sink): Mixer capturing AppSink loopback and Mic loopback
/// 3. **PipeShare_Mic** (remap-source): The final virtual mic facing external apps
/// 4. Target app audios → Moved to PipeShare_AppSink
/// 5. Real mic → Loopback to PipeShare_Mix
/// 6. PipeShare_Mic becomes the new default source
pub async fn create_audio_route(target_app_names: &[String]) -> Result<AudioRoute> {
    let mut module_ids: Vec<u32> = Vec::new();

    // Save current default sink and source
    let prev_source = get_default_source().await.ok();
    let prev_sink = get_default_sink().await.ok();

    info!(
        "[*] Current default microphone: {}",
        prev_source.as_deref().unwrap_or("unknown")
    );
    info!(
        "[*] Current default speaker: {}",
        prev_sink.as_deref().unwrap_or("unknown")
    );

    // ─── Step 1: Create an intermediate Sink for apps (AppSink) ─────────
    let app_sink_id = run_pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=PipeShare_AppSink",
        "sink_properties=device.description=PipeShare_AppSink",
        "channel_map=stereo",
    ])
    .await?;
    if let Ok(mid) = app_sink_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }

    // ─── Step 2: Create Mixer (Null Sink) (App + Mic mixer) ─────────────
    let sink_id = run_pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=PipeShare_Mix",
        "sink_properties=device.description=PipeShare_Mix",
        "channel_map=stereo",
    ])
    .await?;
    if let Ok(mid) = sink_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }
    info!("[+] PipeShare_Mix and PipeShare_AppSink created");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ─── Step 3: Create Virtual Microphone (Remap Source) ───────────────
    let source_id = run_pactl(&[
        "load-module",
        "module-remap-source",
        "source_name=PipeShare_Mic",
        "master=PipeShare_Mix.monitor",
        "source_properties=device.description=PipeShare_Mic",
    ])
    .await?;
    if let Ok(mid) = source_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }
    info!("[+] PipeShare_Mic virtual microphone created");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // ─── Step 4: Establish Loopback Connections ─────────────────────────

    // a) Real microphone -> PipeShare_Mix (Remote hears your voice)
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
    }

    // b) AppSink monitor -> PipeShare_Mix (Remote hears application audio)
    if let Ok(id) = run_pactl(&[
        "load-module",
        "module-loopback",
        "source=PipeShare_AppSink.monitor",
        "sink=PipeShare_Mix",
        "latency_msec=30",
    ])
    .await
    {
        if let Ok(mid) = id.trim().parse::<u32>() {
            module_ids.push(mid);
        }
    }

    // c) AppSink monitor -> Real speaker (User hears application audio locally)
    if let Some(ref speaker) = prev_sink {
        if let Ok(id) = run_pactl(&[
            "load-module",
            "module-loopback",
            "source=PipeShare_AppSink.monitor",
            &format!("sink={}", speaker),
            "latency_msec=30",
        ])
        .await
        {
            if let Ok(mid) = id.trim().parse::<u32>() {
                module_ids.push(mid);
            }
            info!("[+] Local headphone loopback for application audio activated");
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ─── Step 5: Route target application audio (move-sink-input) ───────
    for app_name in target_app_names {
        move_app_to_sink(app_name, "PipeShare_AppSink").await?;
        info!("[+] {} audio routed successfully", app_name);
    }

    // ─── Step 6: Route targeting applications' recording streams (move-source-output)
    // Instead of changing the global default microphone (which crashes WebRTC apps like Element),
    // we find active recording streams (source-inputs) and move them to our virtual mic.
    move_recording_apps_to_mic("PipeShare_Mic.monitor").await?;

    info!("[+] Audio routing completely initialized without disrupting WebRTC!");

    Ok(AudioRoute {
        module_ids,
        target_apps: target_app_names.to_vec(),
        previous_default_source: prev_source, // Kept for struct consistency, but no longer strictly needed
    })
}

/// Helper to create a route for a single application.
pub async fn create_audio_route_single(target_app_name: &str) -> Result<AudioRoute> {
    create_audio_route(&[target_app_name.to_string()]).await
}

/// Fully cleans up an active audio routing session.
///
/// 1. Unloads all PipeShare PulseAudio modules
/// 2. Restores the default microphone to the previous state
pub async fn destroy_audio_route(route: &AudioRoute) -> Result<()> {
    info!("[*] Cleaning up audio route for: {:?}", route.target_apps);

    // Unload modules in reverse order (dependency order)
    for module_id in route.module_ids.iter().rev() {
        match run_pactl(&["unload-module", &module_id.to_string()]).await {
            Ok(_) => info!("[-] Module {} unloaded", module_id),
            Err(e) => error!("[-] Failed to unload module {}: {}", module_id, e),
        }
    }

    /// Restoring default microphone explicitly is no longer needed since we didn't change it.
    info!("[+] Cleanup complete — system returned to normal state");
    Ok(())
}

/// Moves an application's audio stream (sink-input) to the specified virtual sink.
/// This utilizes the PulseAudio/PipeWire rules engine, meaning if a video pauses
/// and resumes, the connection remains linked persistently.
async fn move_app_to_sink(target_app_name: &str, sink_name: &str) -> Result<()> {
    let output = run_pactl(&["list", "sink-inputs"]).await?;
    let mut current_id = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Sink Input #") {
            current_id = trimmed.strip_prefix("Sink Input #").map(|s| s.to_string());
        } else if trimmed.starts_with("application.name =") {
            if let Some(name_str) = trimmed.split('=').nth(1) {
                let name = name_str.trim().trim_matches('"');
                if name == target_app_name || name.starts_with(target_app_name) {
                    if let Some(ref id) = current_id {
                        match run_pactl(&["move-sink-input", id, sink_name]).await {
                            Ok(_) => info!("[+] {} (ID: {}) -> {}", name, id, sink_name),
                            Err(e) => warn!("[-] Failed to move {}: {}", name, e),
                        }
                    }
                }
            }
        }
    }
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
