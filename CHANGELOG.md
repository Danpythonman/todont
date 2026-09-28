# Changelog

## 0.2.0

- **Version checks.** Every sync now carries both sides' todont version
  and a sync protocol number:
  - If the protocols don't match, the server refuses before changing
    anything, and `td` says which side to update.
  - If the versions differ but can still sync, `td sync`, `td init` and
    the TUI mention it.
  - 0.1.0 clients and servers keep working.
- **`td service install` / `uninstall`.** Runs `td serve` as a systemd
  user service.
- **`--system` for always-on machines** such as a Raspberry Pi:
  - `td service install --system` copies `td` to `/usr/local/bin` and runs
    it as a locked-down system service that starts at boot.
  - `td init --server --system` writes the matching config at
    `/etc/todont/config.toml`.
- **Docs.** The README covers running as a service, updating, and reaching
  the server from other devices.

## 0.1.0

First release:
- CLI and TUI with natural-language due dates;
- an add/edit form;
- offline-first sync through `td serve`;
- ntfy reminders and nags, with per-task settings;
- `td init` for first-time setup.
