# PipeShare

![PipeShare Header](https://via.placeholder.com/800x200.png?text=PipeShare+-+Seamless+Audio+Routing+for+Linux)

PipeShare is a modern, lightweight, and event-driven background service for Linux that seamlessly routes application audio during screen sharing sessions. 

Built with Rust, it leverages the power of PipeWire to cleanly mix your microphone with specific application audio streams, bypassing Wayland's security restrictions on third-party application monitoring to provide a flawless, native-feeling screen sharing experience.

## The Problem

On modern Linux desktops running Wayland, security models strictly isolate applications. When you use communication software like Discord, Element, or Microsoft Teams in a browser, they cannot arbitrarily capture the audio output of other applications. Unlike X11, you cannot simply "capture desktop audio" without complex manual routing. 

## The Solution

PipeShare acts as a smart, automated bridge. Running silently as a background daemon, it uses a two-tiered detection strategy (PipeWire graph monitoring + XDG Desktop Portal checks) to instantly detect when a screen casting session starts. 

When you share your screen:
1. **Detection:** PipeShare detects the new ScreenCast video node instantly (with zero polling overhead).
2. **Prompt:** A native dialog prompts you to select which applications' audio you'd like to share.
3. **Routing:** PipeShare dynamically creates virtual PulseAudio modules (via PipeWire) to mix your real microphone with the selected application's audio.
4. **Clean Integration:** This mixed stream is presented as a virtual microphone (`PipeShare_Mic`), which is automatically set as the system default. Your communication app picks it up seamlessly.
5. **Persistence:** Thanks to PipeWire's routing rules (`module-stream-restore`), your application audio remains routed even if you pause a video or switch tabs within the same application.
6. **Cleanup:** When you stop screen sharing, PipeShare detects the end of the session and automatically unloads all virtual devices, returning your system strictly to its previous state.

## Features

- **Event-Driven Architecture:** Zero CPU usage while idling. Reacts instantly to PipeWire graph changes.
- **Split-Route Audio:** The remote party hears the mixed audio, and *you* can still hear the application audio through your own headphones via a local loopback.
- **Robust Persistence:** Automatically handles audio stream interruptions (pausing/playing).
- **Native Prompts:** Uses KDE's `kdialog` (with Zenity fallback) for intuitive user interaction.
- **Memory Safe & Fast:** Written entirely in Rust.

## Prerequisites

- **OS:** Any modern Linux distribution running Wayland.
- **Audio Server:** PipeWire (with `pipewire-pulse` and `wireplumber`).
- **Tools:** `pw-cli`, `pactl` (usually included with PipeWire/PulseAudio installations).
- **UI Dialogs:** `kdialog` (preferred) or `zenity`.
- **D-Bus:** `xdg-desktop-portal` must be active.

## Installation

### From Source

1. Ensure you have Rust and Cargo installed (`rustup`).
2. Clone the repository:
   ```bash
   git clone https://github.com/SavasTanriverdi/PipeShare.git
   cd PipeShare
   ```
3. Build the project:
   ```bash
   cargo build --release
   ```
4. Move the executable to your path:
   ```bash
   sudo cp target/release/pipeshare /usr/local/bin/
   ```

## Usage

PipeShare is designed to run automatically in the background.

### Running the Daemon

To start the daemon manually:
```bash
pipeshare daemon
```

### Systemd Auto-Start (Recommended)

To have PipeShare start automatically when you log in:

1. Copy the provided service file to your user's systemd directory:
   ```bash
   mkdir -p ~/.config/systemd/user/
   cp pipeshare.service ~/.config/systemd/user/
   ```
2. Enable and start the service:
   ```bash
   systemctl --user enable --now pipeshare.service
   ```
3. Check status:
   ```bash
   systemctl --user status pipeshare.service
   ```

### Other Commands

- `pipeshare status` - Display system diagnostics and dependency checks.
- `pipeshare list` - List currently active audio-producing applications.
- `pipeshare route <app>` - Manually bypass the daemon and force route an application.
- `pipeshare stop` - Forcibly unloads all active PipeShare virtual modules.
- `pipeshare monitor` - Observe real-time ScreenCast graph events directly in the terminal.

## Architecture & How It Works

See the source code comments (especially in `src/audio.rs` and `src/dbus_monitor.rs`) for deep technical architecture details regarding our `Null Sink` + `Remap Source` topology and loopback mechanisms.

## License

PipeShare is released under the MIT License.
