# PipeShare

Seamless application audio routing for Linux screen sharing.

PipeShare solves a common problem on Linux Wayland: sharing application audio during screen sharing sessions. Wait for PipeShare to automatically route your application audio alongside your microphone when you start a screen share.

## Features

- Event-driven detection (zero polling) of screen sharing sessions.
- Prompts you to select which applications to share audio from.
- Creates a virtual microphone that mixes real mic and selected app audio.
- Automatically cleans up routing when screen sharing stops.

## Prerequisites

- Linux with Wayland
- PipeWire (`pipewire-pulse` and `wireplumber`)
- `pw-dump`, `pactl`
- `kdialog` or `zenity`
- `xdg-desktop-portal`

## Installation

```bash
git clone https://github.com/SavasTanriverdi/PipeShare.git
cd PipeShare
cargo build --release
sudo cp target/release/pipeshare /usr/local/bin/
```

## Setup

### Auto-start (Recommended)

```bash
mkdir -p ~/.config/systemd/user/
cp pipeshare.service ~/.config/systemd/user/
systemctl --user enable --now pipeshare.service
```

### Manual

```bash
pipeshare daemon
```

## Usage

| Command | Description |
|---------|-------------|
| `pipeshare daemon` | Start as background service |
| `pipeshare list` | Show active audio-producing applications |
| `pipeshare route <app>` | Manually route an app's audio |
| `pipeshare stop` | Force-unload all active routing |
| `pipeshare status` | Display system status |
| `pipeshare monitor` | Watch ScreenCast events |

## License

MIT
