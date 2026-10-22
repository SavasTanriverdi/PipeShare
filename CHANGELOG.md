# Changelog

All notable changes to **PipeShare** are documented in this file.

## [0.5.0] - 2026-10-22

### Added
- **Multi-Compositor XDG Portal Support:** Extended D-Bus portal verification beyond KDE Plasma to automatically detect GNOME (`org.freedesktop.impl.portal.desktop.gnome`), Hyprland (`org.freedesktop.impl.portal.desktop.hyprland`), wlroots/Sway (`org.freedesktop.impl.portal.desktop.wlr`), and GTK portal backends.
- **JSON Output Mode (`pipeshare list --json`):** Added machine-readable JSON output for status bars (Waybar, Polybar) and external automation scripts.
- **CLI Version Command (`pipeshare version` / `--version`):** Dynamic version reporting synced with `Cargo.toml`.
- **Security Hardening & Input Validation:** Added `audio::validate_app_identifier` to sanitize PipeWire node/application identifiers before subprocess invocation, plus `SECURITY.md` documenting IPC trust boundaries.
- **String-Aware Stream Framing:** Hardened `pw-dump --monitor` JSON array depth tracking to ignore `[` and `]` inside quoted application titles (e.g., `Firefox [WebRTC]`).
- **Expanded Unit Test Suite:** Added deterministic CI tests for `pw-dump` lifecycle events (`Started`/`Stopped`/system-node filtering), `pw-link` output parsing, `pactl` device discovery, and `kdialog` checklist parsing.

### Fixed
- Fixed Zenity checklist separator handling for application names containing spaces.
- Added compositor internal stream filters (`mutter`, `gnome-shell`, `xdg-desktop-portal-gnome`, `xdg-desktop-portal-hyprland`, `xdg-desktop-portal-wlr`) to prevent false-positive screen share triggers.

## [0.4.0] - 2026-04-06

### Added
- Exported `move_recording_apps_to_mic` and triggered automatic re-routing during daemon reconnect events.
- Added 3-second WebRTC reconnect debounce window before tearing down virtual sinks.

### Changed
- Initialized virtual sink volumes to 100%, removed redundant virtual microphone indirection, and routed loopback directly to default output.

## [0.3.0] - 2026-03-18

### Changed
- Improved reliability of PipeWire-Pulse null-sink and loopback module teardown ordering.

## [0.2.2] - 2026-03-18

### Fixed
- Audio stability improvements and documentation updates.

## [0.2.1] - 2026-03-18

### Added
- Initial public release with event-driven `pw-dump --monitor` detection and `systemd --user` service integration.
