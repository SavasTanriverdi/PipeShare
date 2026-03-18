# PipeShare

Seamless application audio routing for Linux screen sharing.

## What does it do?

PipeShare is a background service that solves a common problem on Linux: **sharing application audio during screen sharing** on Wayland.

When you share your screen on Discord, Element, or any browser-based communication app, the remote party typically can't hear your application audio — only your microphone. PipeShare fixes that by automatically detecting screen sharing sessions and routing your selected application audio alongside your mic.

No manual PipeWire/PulseAudio configuration needed. It just works.

## How it works

1. You start a screen share in your communication app
2. PipeShare instantly detects it (event-driven, zero polling)
3. A dialog asks which app's audio you want to share
4. PipeShare creates a virtual mic that mixes your real mic + selected app audio
5. When you stop sharing, everything is cleaned up automatically

Under the hood, it captures your selected app's audio through a dedicated virtual sink and loops it back to your speakers automatically. You can freely switch between headphones, HDMI, or any other output device while sharing — the loopback follows your default output.

## Prerequisites

- Linux with Wayland (X11 might work but isn't tested)
- PipeWire with `pipewire-pulse` and `wireplumber`
- `pw-dump`, `pactl` (usually come with PipeWire)
- `kdialog` or `zenity` for the selection dialog
- `xdg-desktop-portal` with a working backend (KDE, GNOME, etc.)

## Installation

```bash
git clone https://github.com/SavasTanriverdi/PipeShare.git
cd PipeShare
cargo build --release
sudo cp target/release/pipeshare /usr/local/bin/
```

## Setup

### Auto-start (recommended)

```bash
mkdir -p ~/.config/systemd/user/
cp pipeshare.service ~/.config/systemd/user/
systemctl --user enable --now pipeshare.service
```

### Manual

```bash
pipeshare daemon
```

## Other commands

| Command | Description |
|---------|-------------|
| `pipeshare status` | System diagnostics and dependency checks |
| `pipeshare list` | Show active audio-producing applications |
| `pipeshare route <app>` | Manually route an app's audio (bypasses daemon) |
| `pipeshare stop` | Force-unload all PipeShare virtual devices |
| `pipeshare monitor` | Watch real-time PipeWire ScreenCast events |

## Security

PipeShare manipulates your system's audio routing, so it's fair to ask what it actually does with your microphone and audio streams. Here's the full picture:

**What PipeShare does:**
- Creates temporary virtual audio devices (`PipeShare_AppSink`, `PipeShare_Mix`, `PipeShare_Mic`) *only* when you actively share your screen and confirm through a dialog
- Moves your selected app's audio to a capture sink, then loops it back to your speakers and to the mixer
- Mixes your real microphone audio into the same virtual mic
- Moves your communication app's recording input to the virtual mic
- Cleans up everything when screen sharing stops

**What PipeShare does NOT do:**
- It never records, stores, or transmits any audio data
- It never activates your microphone on its own — it only redirects an already-active mic stream
- It never runs any network code — all operations are local PipeWire/PulseAudio commands
- It does not modify any system configuration files permanently
- It does not require root privileges to run (only for installation)

**How to verify:**
- The source is fully open — read `src/audio.rs` to see exactly which PulseAudio modules are loaded
- Run `pactl list modules short | grep PipeShare` to see active PipeShare modules at any time
- Run `pw-link -l | grep PipeShare` to see active audio connections
- All virtual devices are removed when sharing stops or when you run `pipeshare stop`

If you have microphones on devices you can't physically mute (webcams, game controllers, etc.), be aware that *any* software with audio routing capabilities could theoretically access them. PipeShare only touches your default microphone and only during active screen sharing sessions that you explicitly confirm.

## Technical details

PipeShare uses a `null-sink` + `remap-source` + `module-loopback` topology:

```
Your App Audio ──► PipeShare_AppSink ──┬──► Loopback → PipeShare_Mix → PipeShare_Mic → Remote
                                      └──► Loopback → Your Speakers (follows default output)
Your Real Mic ────────────────────────────► Loopback → PipeShare_Mix ↗
```

App audio is captured via `pactl move-sink-input` — reliable and persistent. The local playback loopback doesn't specify a target sink, so WirePlumber automatically routes it to your default output and follows device changes.

Screen share detection uses `pw-dump --monitor` which watches PipeWire's graph for new ScreenCast video nodes in real-time, with zero CPU usage while idle.

For more details, see the source code comments in `src/audio.rs` and `src/dbus_monitor.rs`.

## Known limitations

- Only tested on KDE Plasma (Wayland). GNOME should work but hasn't been tested extensively.
- The app selection dialog uses `kdialog` by default with `zenity` as fallback.
- Local playback loopback adds ~30ms latency (not noticeable for most use cases).

## License

MIT
