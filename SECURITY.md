# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.5.x   | :white_check_mark: |
| 0.4.x   | :white_check_mark: |
| < 0.4.0 | :x:                |

## Security Architecture & Trust Boundaries

PipeShare operates as an unprivileged **user-session daemon** (`systemd --user`) and interacts with three primary IPC boundaries on Linux/Wayland systems:

1. **D-Bus Session Bus (`zbus`):**
   - Queries `org.freedesktop.DBus` (`NameHasOwner`) to verify the readiness of `org.freedesktop.portal.Desktop` and compositor-specific `ScreenCast` portal backends (`kde`, `gnome`, `hyprland`, `wlr`, `gtk`).
   - Does not expose any privileged D-Bus interfaces or accept remote commands.

2. **PipeWire Graph Monitoring (`pw-dump --monitor`):**
   - Consumes streaming JSON updates from the local user's PipeWire socket.
   - Untrusted metadata fields from desktop/sandboxed applications (such as `application.name` and `node.name` containing brackets, quotes, or shell metacharacters) are parsed strictly via `serde_json` and validated through `audio::validate_app_identifier` before any routing action.

3. **Subprocess Execution (`pactl`, `pw-link`, `kdialog`, `zenity`, `notify-send`):**
   - PipeShare never invokes a shell (`sh -c`). All external utilities are spawned directly via `tokio::process::Command` with explicit argument vectors (`args`), preventing shell command injection.
   - Application identifiers are validated against control characters, excessive length, and injection metacharacters prior to virtual sink linking.

## Reporting a Vulnerability

If you discover a security vulnerability (such as privilege escalation, argument injection via crafted PipeWire node properties, or denial-of-service in the event monitor), please report it privately:

- **Email:** [savastnr@duck.com](mailto:savastnr@duck.com)
- **GitHub Security Advisories:** Use the ["Report a vulnerability"](https://github.com/SavasTanriverdi/PipeShare/security/advisories/new) tab on this repository.

We aim to acknowledge reports within **48 hours** and provide a remediation release within **7 days**.
