#!/usr/bin/env bash
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${HOME}/qwertdvert"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
APPLICATIONS_DIR="${HOME}/.local/share/applications"

UDEV_RULE_PATH="/etc/udev/rules.d/70-qwertdvert.rules"

usage() {
  cat <<'EOF'
Usage:
  scripts/qwertdvert-manage.sh install [--no-build] [--enable-autostart]
  scripts/qwertdvert-manage.sh uninstall
  scripts/qwertdvert-manage.sh status

Notes:
- Installs into ~/qwertdvert
- Uses systemd --user units for autostart and lifecycle
- Installs a udev rule for uaccess on keyboard event devices + /dev/uinput
- Installs a KDE app-launcher entry into ~/.local/share/applications
EOF
}

refresh_desktop_database() {
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
  fi
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "ERROR: missing required command: $1" >&2
    exit 1
  }
}

require_sudo_noninteractive() {
  require_cmd sudo
  if ! sudo -n true 2>/dev/null; then
    echo "ERROR: sudo privileges are required for udev rule changes." >&2
    echo "Run: sudo -v" >&2
    echo "Then re-run this command." >&2
    exit 1
  fi
}

install_udev_rule() {
  if [[ -e "$UDEV_RULE_PATH" ]] && command -v diff >/dev/null 2>&1; then
    if diff -q "$UDEV_RULE_PATH" <(cat <<'EOF'
# QwertDvert: allow the active (logged-in) desktop user to access the keyboard
# event device nodes (for EVIOCGRAB/read) and /dev/uinput (for writing).
#
# This uses systemd-logind's 'uaccess' mechanism, which grants ACLs to the active seat user.
# If you change this file, run: sudo udevadm control --reload-rules && sudo udevadm trigger

# Keyboards only (limit blast radius)
SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_KEYBOARD}=="1", TAG+="uaccess"

# Virtual input injection
KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess"
EOF
    ) >/dev/null 2>&1; then
      echo "udev rule already up to date; skipping."
      return
    fi
  fi

  require_sudo_noninteractive
  sudo -n tee "$UDEV_RULE_PATH" >/dev/null <<'EOF'
# QwertDvert: allow the active (logged-in) desktop user to access the keyboard
# event device nodes (for EVIOCGRAB/read) and /dev/uinput (for writing).
#
# This uses systemd-logind's 'uaccess' mechanism, which grants ACLs to the active seat user.
# If you change this file, run: sudo udevadm control --reload-rules && sudo udevadm trigger

# Keyboards only (limit blast radius)
SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_KEYBOARD}=="1", TAG+="uaccess"

# Virtual input injection
KERNEL=="uinput", SUBSYSTEM=="misc", TAG+="uaccess"
EOF

  sudo -n udevadm control --reload-rules
  sudo -n udevadm trigger
}

remove_udev_rule() {
  if [[ -e "$UDEV_RULE_PATH" ]]; then
    require_sudo_noninteractive
    sudo -n rm -f "$UDEV_RULE_PATH"
    sudo -n udevadm control --reload-rules
    sudo -n udevadm trigger
  fi
}

install() {
  require_cmd systemctl

  local do_build=1
  local enable_autostart=0

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-build)
        do_build=0
        shift
        ;;
      --enable-autostart)
        enable_autostart=1
        shift
        ;;
      *)
        echo "ERROR: unexpected install argument: $1" >&2
        usage
        exit 2
        ;;
    esac
  done

  if [[ $do_build -eq 1 ]]; then
    require_cmd cargo
    echo "Building (release)…"
    (cd "$REPO_DIR" && cargo build --release)
  else
    if [[ ! -x "$REPO_DIR/target/release/qwertdvert" || ! -x "$REPO_DIR/target/release/qwertdvert-tray" ]]; then
      echo "ERROR: release binaries not found in target/release." >&2
      echo "Run: cargo build --release" >&2
      exit 1
    fi
  fi

  echo "Installing binaries to $INSTALL_DIR…"
  mkdir -p "$INSTALL_DIR"
  cp -f "$REPO_DIR/target/release/qwertdvert" "$REPO_DIR/target/release/qwertdvert-tray" "$INSTALL_DIR/"

  echo "Installing systemd user units…"
  mkdir -p "$SYSTEMD_USER_DIR"
  cp -a "$REPO_DIR/systemd/user/." "$SYSTEMD_USER_DIR/"
  systemctl --user daemon-reload

  echo "Installing desktop launcher…"
  mkdir -p "$APPLICATIONS_DIR"
  cp -f "$REPO_DIR/qwertdvert.desktop" "$APPLICATIONS_DIR/"
  refresh_desktop_database

  echo "Installing udev rule (requires sudo)…"
  install_udev_rule

  if [[ $enable_autostart -eq 1 ]]; then
    echo "Enabling + starting qwertdvert.target…"
    systemctl --user enable --now qwertdvert.target
  else
    echo "Not enabling autostart (opt-in)."
    echo "Start it from the app launcher or run: systemctl --user start qwertdvert.target"
    echo "To enable autostart: systemctl --user enable --now qwertdvert.target"
  fi

  echo
  echo "Installed. Status:"
  systemctl --user status qwertdvert.target qwertdvert-daemon.service qwertdvert-tray.service --no-pager || true
}

status() {
  require_cmd systemctl

  echo "Installed binaries:"
  if [[ -d "$INSTALL_DIR" ]]; then
    ls -la "$INSTALL_DIR" | sed -n '1,120p'
  else
    echo "  (missing) $INSTALL_DIR"
  fi

  echo
  echo "udev rule:"
  if [[ -e "$UDEV_RULE_PATH" ]]; then
    echo "  present: $UDEV_RULE_PATH"
  else
    echo "  missing: $UDEV_RULE_PATH"
  fi

  echo
  echo "systemd --user status:"
  systemctl --user status qwertdvert.target qwertdvert-daemon.service qwertdvert-tray.service --no-pager || true

  if command -v journalctl >/dev/null 2>&1; then
    echo
    echo "Recent daemon logs:"
    journalctl --user -u qwertdvert-daemon.service -n 40 --no-pager || true

    echo
    echo "Recent tray logs:"
    journalctl --user -u qwertdvert-tray.service -n 40 --no-pager || true
  fi
}

uninstall() {
  require_cmd systemctl

  echo "Stopping/disabling services…"
  systemctl --user disable --now qwertdvert.target 2>/dev/null || true
  systemctl --user stop qwertdvert-daemon.service qwertdvert-tray.service 2>/dev/null || true

  echo "Removing systemd user units…"
  rm -f "$SYSTEMD_USER_DIR/qwertdvert.target" \
        "$SYSTEMD_USER_DIR/qwertdvert-daemon.service" \
        "$SYSTEMD_USER_DIR/qwertdvert-tray.service" \
        "$SYSTEMD_USER_DIR/default.target.wants/qwertdvert.target"
  systemctl --user daemon-reload

  echo "Removing installed binaries…"
  rm -rf "$INSTALL_DIR"

  echo "Removing desktop launcher…"
  rm -f "$APPLICATIONS_DIR/qwertdvert.desktop"
  refresh_desktop_database

  echo "Removing udev rule (requires sudo if present)…"
  remove_udev_rule

  echo "Uninstalled."
}

main() {
  if [[ $# -lt 1 ]]; then
    usage
    exit 2
  fi

  case "$1" in
    install)
      shift
      install "$@"
      ;;
    uninstall) uninstall ;;
    status) status ;;
    -h|--help|help) usage ;;
    *)
      echo "ERROR: unknown command: $1" >&2
      usage
      exit 2
      ;;
  esac
}

main "$@"
