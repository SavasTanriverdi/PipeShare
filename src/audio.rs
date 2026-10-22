//! PipeShare Audio Routing Engine
//! Creates virtual audio devices via PipeWire-Pulse interface, mixing
//! application audio and real microphone audio into a single virtual microphone.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

/// Represents an audio node found in PipeWire.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Created PulseAudio module IDs -- stored for cleanup
    pub module_ids: Vec<u32>,
    /// Target application names being routed
    pub target_apps: Vec<String>,
    /// Previous default source prior to routing (to restore later)
    #[allow(dead_code)]
    pub previous_default_source: Option<String>,
}

/// Validates an application or audio device identifier before passing it to subprocess tools.
/// Rejects empty strings, control characters, and shell/property injection characters.
pub fn validate_app_identifier(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Application identifier cannot be empty");
    }
    if trimmed.len() > 128 {
        anyhow::bail!("Application identifier exceeds maximum length (128 chars)");
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, ';' | '|' | '&' | '$' | '`' | '\0'))
    {
        anyhow::bail!("Application identifier contains invalid or unsafe characters");
    }
    Ok(trimmed)
}

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

/// Parses the output of `pw-link -o` into deduplicated `AudioNode` items.
pub fn parse_pw_link_output(output: &str) -> Vec<AudioNode> {
    let mut nodes: HashMap<String, AudioNode> = HashMap::new();
    let mut ordered_keys: Vec<String> = Vec::new();
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
            if app_name.is_empty() {
                continue;
            }

            if FILTERED_PREFIXES
                .iter()
                .any(|prefix| app_name.starts_with(prefix))
            {
                continue;
            }

            if !nodes.contains_key(&app_name) {
                ordered_keys.push(app_name.clone());
                nodes.insert(
                    app_name.clone(),
                    AudioNode {
                        id: next_id,
                        app_name,
                    },
                );
                next_id += 1;
            }
        }
    }

    ordered_keys
        .into_iter()
        .filter_map(|k| nodes.remove(&k))
        .collect()
}

/// Lists applications currently producing audio on the system.
pub async fn list_audio_sources() -> Result<Vec<AudioNode>> {
    let output = run_pw_link(&["-o"]).await?;
    let result = parse_pw_link_output(&output);
    info!("Found {} audio sources", result.len());
    Ok(result)
}

/// Parses a specific header value (e.g. `Default Source:`) from `pactl info` output.
pub fn parse_pactl_info_field(output: &str, field_prefix: &str) -> Option<String> {
    for line in output.lines() {
        if line.contains(field_prefix) {
            let value = line.split(':').nth(1).unwrap_or("").trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Retrieves the system's current default source (microphone).
async fn get_default_source() -> Result<String> {
    let output = run_pactl(&["info"]).await?;
    parse_pactl_info_field(&output, "Default Source:")
        .ok_or_else(|| anyhow::anyhow!("Default audio source not found"))
}

/// Retrieves the system's current default sink (speaker/headphones).
#[allow(dead_code)]
async fn get_default_sink() -> Result<String> {
    let output = run_pactl(&["info"]).await?;
    parse_pactl_info_field(&output, "Default Sink:")
        .ok_or_else(|| anyhow::anyhow!("Default audio sink not found"))
}

/// Creates a complete audio route combining selected applications' audio
/// and the real microphone.
pub async fn create_audio_route(target_app_names: &[String]) -> Result<AudioRoute> {
    for app_name in target_app_names {
        validate_app_identifier(app_name)?;
    }

    let mut module_ids: Vec<u32> = Vec::new();

    let prev_source = get_default_source().await.ok();

    info!(
        "Current default microphone: {}",
        prev_source.as_deref().unwrap_or("unknown")
    );

    // Step 1: Create PipeShare_AppSink
    let appsink_id = run_pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=PipeShare_AppSink",
        r#"sink_properties="device.description=PipeShare_AppSink device.class=filter node.virtual=true""#,
        "channel_map=stereo",
    ])
    .await?;
    if let Ok(mid) = appsink_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }
    let _ = run_pactl(&["set-sink-mute", "PipeShare_AppSink", "0"]).await;
    let _ = run_pactl(&["set-sink-volume", "PipeShare_AppSink", "100%"]).await;
    info!("PipeShare_AppSink created");

    // Step 2: Create PipeShare_Mix
    let mix_id = run_pactl(&[
        "load-module",
        "module-null-sink",
        "sink_name=PipeShare_Mix",
        r#"sink_properties="device.description=PipeShare_Mix device.class=filter node.virtual=true""#,
        "channel_map=stereo",
    ])
    .await?;
    if let Ok(mid) = mix_id.trim().parse::<u32>() {
        module_ids.push(mid);
    }
    let _ = run_pactl(&["set-sink-mute", "PipeShare_Mix", "0"]).await;
    let _ = run_pactl(&["set-sink-volume", "PipeShare_Mix", "100%"]).await;
    info!("PipeShare_Mix created");

    // Step 3: Real microphone -> PipeShare_Mix
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
        info!("Microphone loopback to PipeShare_Mix established");
    }

    // Step 4: AppSink.monitor -> PipeShare_Mix
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
    info!("App audio -> PipeShare_Mix loopback established");

    // Step 5: AppSink.monitor -> User's Speaker
    let default_sink = get_default_sink().await.ok();
    let sink_arg = default_sink.map(|s| format!("sink={}", s));

    let mut args = vec![
        "load-module",
        "module-loopback",
        "source=PipeShare_AppSink.monitor",
        "latency_msec=30",
    ];
    if let Some(ref s) = sink_arg {
        args.push(s);
    }

    if let Ok(id) = run_pactl(&args).await {
        if let Ok(mid) = id.trim().parse::<u32>() {
            module_ids.push(mid);
        }
    }
    info!("Local playback loopback established (follows default output)");

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Step 6: Move app audio to PipeShare_AppSink
    for app_name in target_app_names {
        move_app_to_appsink(app_name).await;
    }

    // Step 7: Route recording streams to the Mix monitor directly
    move_recording_apps_to_mic("PipeShare_Mix.monitor").await?;

    info!("Audio routing initialized -- output device switching is fully supported!");

    Ok(AudioRoute {
        module_ids,
        target_apps: target_app_names.to_vec(),
        previous_default_source: prev_source,
    })
}

/// Moves an application's sink-inputs to PipeShare_AppSink.
async fn move_app_to_appsink(target_app_name: &str) {
    let Ok(output) = run_pactl(&["list", "sink-inputs"]).await else {
        return;
    };

    let mut current_id: Option<String> = None;
    let mut moved = 0;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Sink Input #") {
            current_id = trimmed.strip_prefix("Sink Input #").map(|s| s.to_string());
        } else if trimmed.starts_with("application.name =") || trimmed.starts_with("node.name =") {
            if let Some(name_str) = trimmed.split('=').nth(1) {
                let name = name_str.trim().trim_matches('"');
                if name
                    .to_lowercase()
                    .contains(&target_app_name.to_lowercase())
                {
                    if let Some(ref id) = current_id {
                        match run_pactl(&["move-sink-input", id, "PipeShare_AppSink"]).await {
                            Ok(_) => {
                                info!("Moved '{}' (sink-input {}) to PipeShare_AppSink", name, id);
                                moved += 1;
                            }
                            Err(e) => debug!("Failed to move {}: {}", name, e),
                        }
                    }
                }
            }
        }
    }

    if moved == 0 {
        warn!("No sink-inputs found for '{}'", target_app_name);
    }
}

/// Public wrapper to re-move app audio after a screen share restart.
pub async fn relink_app_to_mix(target_app_name: &str) -> Result<()> {
    validate_app_identifier(target_app_name)?;
    move_app_to_appsink(target_app_name).await;
    Ok(())
}

/// Moves active recording streams (source-outputs) to our virtual microphone.
/// This targets WebRTC applications (Element, Discord, Firefox, Chrome) and routes
/// them silently to `PipeShare_Mix.monitor` without touching the global default microphone.
pub async fn move_recording_apps_to_mic(virtual_mic_name: &str) -> Result<()> {
    let output = run_pactl(&["list", "source-outputs"]).await?;
    let mut current_id = None;

    for line in output.lines() {
        let trimmed = line.trim();

        // Find the recording stream ID
        if trimmed.starts_with("Source Output #") {
            current_id = trimmed
                .strip_prefix("Source Output #")
                .map(|s| s.to_string());
        }
        // Identify the application name
        else if trimmed.starts_with("application.name =") {
            if let Some(name_str) = trimmed.split('=').nth(1) {
                let name = name_str.trim().trim_matches('"');

                // We want to move communication apps that are actively recording.
                // We exclude system processes or PipeWire's own loops to prevent feedback loops.
                if !FILTERED_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                {
                    if let Some(ref id) = current_id {
                        match run_pactl(&["move-source-output", id, virtual_mic_name]).await {
                            Ok(_) => info!(
                                "Routed recording app '{}' (ID: {}) to {}",
                                name, id, virtual_mic_name
                            ),
                            Err(e) => debug!("Skipping move for recording app {}: {}", name, e),
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
    validate_app_identifier(target_app_name)?;
    create_audio_route(&[target_app_name.to_string()]).await
}

/// Fully cleans up an active audio routing session.
///
/// 1. Restores recording streams to the default mic FIRST
/// 2. Unloads all PipeShare PulseAudio modules
/// 3. pw-link connections auto-disconnect when the null-sink is destroyed
pub async fn destroy_audio_route(route: &AudioRoute) -> Result<()> {
    info!("Cleaning up audio route for: {:?}", route.target_apps);

    // CRITICAL: Restore recording streams BEFORE destroying modules.
    // If we destroy PipeShare_Mix first, WebRTC source-outputs
    // would point to a non-existent source, breaking the capture pipeline.
    restore_recording_streams_to_default().await;

    // Unload modules in reverse order (dependency order)
    for module_id in route.module_ids.iter().rev() {
        match run_pactl(&["unload-module", &module_id.to_string()]).await {
            Ok(_) => info!("Module {} unloaded", module_id),
            Err(e) => error!("Failed to unload module {}: {}", module_id, e),
        }
    }

    info!("Cleanup complete.");
    Ok(())
}

/// Moves all recording streams currently using PipeShare sources back to
/// the system's default microphone. Must be called BEFORE unloading modules.
async fn restore_recording_streams_to_default() {
    let default_source = match get_default_source().await {
        Ok(s) => s,
        Err(_) => {
            warn!("Could not determine default source for restoration");
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
            current_id = trimmed
                .strip_prefix("Source Output #")
                .map(|s| s.to_string());
        } else if trimmed.contains("PipeShare") || trimmed.contains("pipeshare") {
            // This source-output is connected to a PipeShare source -- restore it
            if let Some(ref id) = current_id {
                match run_pactl(&["move-source-output", id, &default_source]).await {
                    Ok(_) => info!("Restored recording stream {} to {}", id, default_source),
                    Err(e) => debug!("Could not restore stream {}: {}", id, e),
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
                info!("Module {} unloaded", id_str);
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
                info!("Default microphone restored: {}", name);
                break;
            }
        }
    }

    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_app_identifier() {
        assert_eq!(validate_app_identifier("Firefox").unwrap(), "Firefox");
        assert_eq!(
            validate_app_identifier("  VLC media player  ").unwrap(),
            "VLC media player"
        );
        assert!(validate_app_identifier("").is_err());
        assert!(validate_app_identifier("Firefox; rm -rf /").is_err());
        assert!(validate_app_identifier("App\nInjected").is_err());
    }

    #[test]
    fn test_parse_pw_link_output() {
        let sample = "\
alsa_output.pci-0000_00_1f.3.analog-stereo:monitor_FL
Firefox:output_FL
Firefox:output_FR
Spotify:output_FL
PipeShare_Mix:monitor_FL
WEBRTC VoiceEngine:capture_FL
";
        let nodes = parse_pw_link_output(sample);
        assert_eq!(
            nodes,
            vec![
                AudioNode {
                    id: 1,
                    app_name: "Firefox".to_string(),
                },
                AudioNode {
                    id: 2,
                    app_name: "Spotify".to_string(),
                },
            ]
        );
    }

    #[test]
    fn test_parse_pactl_info_field() {
        let info = "\
Server Name: PulseAudio (on PipeWire 1.2.4)
Default Sink: alsa_output.pci-0000_00_1f.3.analog-stereo
Default Source: alsa_input.usb-Focusrite_Scarlett-00.mono-fallback
";
        assert_eq!(
            parse_pactl_info_field(info, "Default Source:"),
            Some("alsa_input.usb-Focusrite_Scarlett-00.mono-fallback".to_string())
        );
        assert_eq!(
            parse_pactl_info_field(info, "Default Sink:"),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo".to_string())
        );
    }

    #[tokio::test]
    async fn test_list_audio_sources() {
        let result = list_audio_sources().await;
        // Skip gracefully in CI where PipeWire isn't available
        if result.is_err() {
            eprintln!("Skipping: PipeWire/PulseAudio not available");
            return;
        }
        let sources = result.unwrap();
        println!("Found {} audio sources", sources.len());
    }

    #[tokio::test]
    async fn test_get_default_source() {
        let result = get_default_source().await;
        if result.is_err() {
            eprintln!("Skipping: PulseAudio not available");
            return;
        }
        let source = result.unwrap();
        assert!(!source.is_empty(), "Default source should not be empty");
        println!("Default source: {}", source);
    }
}
