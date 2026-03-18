// ─────────────────────────────────────────────────────────────────────────────
// PipeShare — Seamless Audio Sharing for Linux
// ─────────────────────────────────────────────────────────────────────────────
//! A modern, lightweight background service for Linux that seamlessly
//! shares application audio during screen sharing sessions.
//!
//! Usage:
//!   pipeshare daemon         — Start as a background service (primary mode)
//!   pipeshare list           — List audio-producing applications
//!   pipeshare route <app>    — Manually route audio
//!   pipeshare stop           — Stop all active routes
//!   pipeshare status         — Display system status
//!   pipeshare monitor        — Monitor screen sharing events live

mod audio;
mod daemon;
mod dbus_monitor;

use anyhow::Result;
use tracing::error;
use tracing_subscriber::EnvFilter;

// ─── Banner ──────────────────────────────────────────────────────────────────

const BANNER: &str = r#"
================================================================================
  PipeShare v0.2.1
  Seamless Audio Sharing for Linux
================================================================================
"#;

// ─── Main Entry ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("help");

    match command {
        "daemon" | "start" => cmd_daemon().await?,
        "list" => cmd_list().await?,
        "route" => {
            let app_name = args.get(2).map(String::as_str).unwrap_or_else(|| {
                eprintln!("Error: Usage: pipeshare route <application_name>");
                eprintln!("       Run 'pipeshare list' to see available applications first.");
                std::process::exit(1);
            });
            cmd_route(app_name).await?;
        }
        "stop" => cmd_stop().await?,
        "status" => cmd_status().await?,
        "monitor" => cmd_monitor().await?,
        "help" | "--help" | "-h" => cmd_help(),
        _ => {
            eprintln!("Error: Unknown command: '{}'", command);
            cmd_help();
            std::process::exit(1);
        }
    }

    Ok(())
}

// ─── Commands ────────────────────────────────────────────────────────────────

/// `pipeshare daemon` — Primary mode: runs as a background service.
///
/// Monitors the PipeWire graph, displays a dialog to the user when screen sharing
/// is detected, routes audio, and cleans up when sharing stops.
async fn cmd_daemon() -> Result<()> {
    println!("{}", BANNER);
    println!("[*] Starting daemon mode...\n");

    // Graceful shutdown on Ctrl+C
    let shutdown = tokio::spawn(async {
        tokio::signal::ctrl_c().await.ok();
        println!("\n[*] Shutting down daemon...");
        // Cleanup
        let _ = audio::cleanup_all().await;
        println!("[+] Clean shutdown completed.");
        std::process::exit(0);
    });

    // Start daemon
    tokio::select! {
        result = daemon::run_daemon() => {
            if let Err(e) = result {
                error!("Daemon error: {}", e);
            }
        }
        _ = shutdown => {}
    }

    Ok(())
}

/// `pipeshare list` — Lists active audio sources.
async fn cmd_list() -> Result<()> {
    println!("{}", BANNER);
    println!("[*] Scanning system for audio-producing applications...\n");

    let sources = audio::list_audio_sources().await?;

    if sources.is_empty() {
        println!("[-] No audio-producing applications found currently.");
        println!("    Hint: Start playing a video in a browser or open a music player.");
    } else {
        println!("  ┌─────┬────────────────────────────────────┐");
        println!("  │  #  │ Application                        │");
        println!("  ├─────┼────────────────────────────────────┤");
        for (i, source) in sources.iter().enumerate() {
            println!("  │ {:>3} │ {:<34} │", i + 1, source.app_name);
        }
        println!("  └─────┴────────────────────────────────────┘");
        println!();
        println!(
            "    Hint: run `pipeshare route \"{}\"`",
            sources[0].app_name
        );
    }

    Ok(())
}

/// `pipeshare route <app>` — Manual audio routing.
async fn cmd_route(app_name: &str) -> Result<()> {
    println!("{}", BANNER);
    println!("[*] Initializing manual audio routing for: {}\n", app_name);

    let route = audio::create_audio_route_single(app_name).await?;

    println!();
    println!("[+] Audio routing ACTIVE");
    println!("    Source:   {}", app_name);
    println!("    Mic:      Mixed (System microphone + Application audio)");
    println!("    Target:   PipeShare_Mic (Set as the default microphone)");
    println!();
    println!("    Note: Your communication app (e.g., Discord/Element) should now");
    println!("          automatically use 'PipeShare_Mic'.");
    println!();
    println!("    Press Ctrl+C or run 'pipeshare stop' to terminate.");

    // Wait for Ctrl+C to trigger cleanup
    tokio::signal::ctrl_c().await?;

    println!("\n\n[*] Stopping routing...");
    audio::destroy_audio_route(&route).await?;

    Ok(())
}

/// `pipeshare stop` — Cleans up all PipeShare modules.
async fn cmd_stop() -> Result<()> {
    println!("{}", BANNER);
    println!("[*] Cleaning up PipeShare virtual devices...\n");

    let cleaned = audio::cleanup_all().await?;

    if cleaned == 0 {
        println!("[-] No active PipeShare modules found to clean.");
    } else {
        println!("\n[+] Successfully cleaned {} module(s).", cleaned);
    }

    Ok(())
}

/// `pipeshare status` — Displays system status and dependencies.
async fn cmd_status() -> Result<()> {
    println!("{}", BANNER);
    println!("System Status\n");

    // PipeWire
    let pw_version = tokio::process::Command::new("pw-cli")
        .args(["--version"])
        .output()
        .await;
    match pw_version {
        Ok(out) => {
            let ver = String::from_utf8_lossy(&out.stdout);
            let version_line = ver.lines().last().unwrap_or("unknown");
            println!("  [+] PipeWire       : {}", version_line.trim());
        }
        Err(_) => println!("  [-] PipeWire       : Not found!"),
    }

    // WirePlumber
    let wp = tokio::process::Command::new("wpctl")
        .args(["status"])
        .output()
        .await;
    match wp {
        Ok(out) if out.status.success() => println!("  [+] WirePlumber    : Active"),
        _ => println!("  [-] WirePlumber    : Not found"),
    }

    // XDG Portal
    let portal = dbus_monitor::check_portal_available().await?;
    println!(
        "  {} XDG Portal      : {}",
        if portal { "[+]" } else { "[-]" },
        if portal { "Active" } else { "Not found" }
    );

    // kdialog
    let kd = tokio::process::Command::new("kdialog")
        .args(["--version"])
        .output()
        .await;
    match kd {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout);
            println!("  [+] kdialog        : {}", ver.trim());
        }
        _ => println!("  [-] kdialog        : Not found (dialog prompts will fail back)"),
    }

    // pactl
    let pactl = tokio::process::Command::new("pactl")
        .args(["--version"])
        .output()
        .await;
    match pactl {
        Ok(out) => {
            let ver = String::from_utf8_lossy(&out.stdout);
            println!("  [+] pactl          : {}", ver.trim());
        }
        Err(_) => println!("  [-] pactl          : Not found"),
    }

    // Active PipeShare sessions
    println!("\nActive PipeShare Sessions:\n");

    let mods = tokio::process::Command::new("pactl")
        .args(["list", "modules", "short"])
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&mods.stdout);
    let mut active = 0;
    for line in stdout.lines() {
        if line.contains("PipeShare") || line.contains("pipeshare") {
            println!("  * {}", line);
            active += 1;
        }
    }
    if active == 0 {
        println!("  [-] No active PipeShare sessions detected.");
    }

    Ok(())
}

/// `pipeshare monitor` — Live monitor for screen sharing events via D-Bus.
async fn cmd_monitor() -> Result<()> {
    println!("{}", BANNER);
    println!("[*] Starting event-based screen sharing monitor...");
    println!("    (No polling — sleeps until graph changes)");
    println!("    Press Ctrl+C to stop.\n");

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);

    tokio::spawn(async move {
        if let Err(e) = dbus_monitor::monitor_screen_share(tx).await {
            error!("Monitor error: {}", e);
        }
    });

    while let Some(event) = rx.recv().await {
        match event {
            dbus_monitor::ScreenShareEvent::Started { app_name, node_id } => {
                let name = app_name.unwrap_or_else(|| "Unknown".to_string());
                println!("[{}] Screen sharing STARTED", chrono_now());
                println!("    Application : {}", name);
                println!("    Node ID     : {}", node_id);
                println!("    Hint: use `pipeshare route <app>` to route audio manually.");
            }
            dbus_monitor::ScreenShareEvent::Stopped { node_id } => {
                println!(
                    "[{}] Screen sharing STOPPED (node: {})",
                    chrono_now(),
                    node_id
                );
            }
        }
    }

    Ok(())
}

/// `pipeshare help` — Display help information.
fn cmd_help() {
    println!("{}", BANNER);
    println!("Usage: pipeshare <command> [options]\n");
    println!("Commands:");
    println!("  daemon            Start as a background service (primary mode)");
    println!("  list              List audio-producing applications");
    println!("  route <app>       Start manual audio routing");
    println!("  stop              Stop all routing and clean up virtualization");
    println!("  status            Display system state and dependencies");
    println!("  monitor           Monitor screen sharing events live");
    println!("  help              Display this help message");
    println!();
    println!("Examples:");
    println!("  pipeshare daemon");
    println!("  pipeshare list");
    println!("  pipeshare route Firefox");
    println!("  pipeshare stop");
    println!();
    println!("Systemd Automatic Startup:");
    println!("  systemctl --user enable --now pipeshare.service");
    println!();
    println!("Environment Variables:");
    println!("  RUST_LOG=debug    Enable verbose logging");
}

// ─── Utilities ───────────────────────────────────────────────────────────────

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, s)
}
