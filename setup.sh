#!/bin/bash
# ==========================================
# baan setup / uninstall script
# Builds the release binary and installs it
# along with the systemd template unit and a
# system-wide config, then starts the daemon
# for the selected keyboard.
#
# Run as a normal user; sudo is used only
# for the privileged steps (file installs,
# systemctl, …) and will prompt for the
# password when needed.
# ==========================================
set -euo pipefail

PREFIX="/usr/local"
SERVICE_DIR="/etc/systemd/system"
CONFIG_DIR="/etc/baan"
CONFIG_SRC=""
KEYBOARD=""
ENABLE_SERVICE=1
DO_INSTALL=0
DO_UNINSTALL=0

# ==========================================
# usage / arg parsing
# ==========================================
usage() {
    cat <<EOF
Usage: $0 {--install|--uninstall} [OPTIONS]

Actions:
      --install          Build and install baan, then start the service.
      --uninstall        Disable/stop the service and remove installed files.

Install options:
  -k, --keyboard PATH   Keyboard device node, e.g. event3 or /dev/input/event3.
                        Auto-detected if omitted.
  -c, --config PATH     Source config file to install to /etc/baan/baan.toml.
                        Default: baan.example.toml.
      --prefix PATH     Install prefix for the binary (default: $PREFIX).
      --no-start        Install but do not enable/start the service.
  -h, --help            Show this help message.

Examples:
  $0 --install
  $0 --install -k event3
  $0 --install -c ./baan.example.toml --no-start
  $0 --uninstall
EOF
    exit 1
}

while [[ "$#" -gt 0 ]]; do
    case $1 in
        -k|--keyboard)
            KEYBOARD="${2:-}"
            if [[ -z "$KEYBOARD" ]]; then
                echo "Error: -k/--keyboard requires a path argument." >&2
                exit 1
            fi
            shift
            ;;
        -c|--config)
            CONFIG_SRC="${2:-}"
            if [[ -z "$CONFIG_SRC" ]]; then
                echo "Error: -c/--config requires a path argument." >&2
                exit 1
            fi
            shift
            ;;
        --prefix)
            PREFIX="${2:-}"
            if [[ -z "$PREFIX" ]]; then
                echo "Error: --prefix requires a path argument." >&2
                exit 1
            fi
            shift
            ;;
        --no-start)
            ENABLE_SERVICE=0
            ;;
        --install)
            DO_INSTALL=1
            ;;
        --uninstall)
            DO_UNINSTALL=1
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Error: Unknown option '$1'" >&2
            usage
            ;;
    esac
    shift
done

if [[ "$DO_INSTALL" -eq 1 && "$DO_UNINSTALL" -eq 1 ]]; then
    echo "Error: --install and --uninstall are mutually exclusive." >&2
    exit 1
fi
if [[ "$DO_INSTALL" -eq 0 && "$DO_UNINSTALL" -eq 0 ]]; then
    echo "Error: specify either --install or --uninstall." >&2
    usage
fi

# ==========================================
# helpers
# ==========================================
# Run a command with sudo unless we're already root.
run_priv() {
    if [[ "$EUID" -eq 0 ]]; then
        "$@"
    else
        sudo "$@"
    fi
}

normalize_event() {
    local node="${1##*/}"   # strip any leading /dev/input/
    if [[ "$node" != event* ]]; then
        echo "Error: '$1' does not look like an event device node (expected eventN)." >&2
        exit 1
    fi
    echo "$node"
}

# Non-interactive keyboard detection from /proc/bus/input/devices.
# Prefers a device whose name contains "keyboard"; otherwise the first
# device with a kbd handler.
detect_keyboard() {
    local name="" candidate=""
    while IFS= read -r line; do
        if [[ $line == *"N: Name="* ]]; then
            name="${line#*N: Name=\"}"
            name="${name%\"*}"
        fi
        if [[ $line == *"H: Handlers="* && $line == *"kbd"* ]]; then
            local event
            event=$(echo "$line" | grep -o 'event[0-9]*' | head -n1)
            if [[ -n "$event" ]]; then
                if [[ -z "$candidate" ]]; then
                    candidate="$event"
                fi
                if [[ "${name,,}" == *"keyboard"* ]]; then
                    echo "$event"
                    return 0
                fi
            fi
        fi
    done < /proc/bus/input/devices

    if [[ -z "$candidate" ]]; then
        echo "Error: No keyboard device found in /proc/bus/input/devices." >&2
        echo "       Pass one explicitly with -k eventN." >&2
        exit 1
    fi

    echo "$candidate"
    return 0
}

# ==========================================
# uninstall
# ==========================================
uninstall() {
    echo "Stopping and disabling baan services..."
    mapfile -t units < <(systemctl list-unit-files --type=service --no-legend 2>/dev/null \
        | grep '^baan@.*\.service' \
        | awk '{print $1}' || true)
    if [[ ${#units[@]} -gt 0 ]]; then
        run_priv systemctl stop "${units[@]}" || true
        run_priv systemctl disable "${units[@]}" || true
    fi

    echo "Removing service unit..."
    run_priv rm -f "$SERVICE_DIR/baan@.service"
    run_priv systemctl daemon-reload

    echo "Removing binary..."
    run_priv rm -f "$PREFIX/bin/baan"

    echo "Removing config..."
    run_priv rm -f "$CONFIG_DIR/baan.toml"
    run_priv rmdir "$CONFIG_DIR" 2>/dev/null || true

    echo "Done. Config at $CONFIG_DIR was removed; restore it if needed."
    exit 0
}

# ==========================================
# main
# ==========================================
if [[ "$DO_UNINSTALL" -eq 1 ]]; then
    uninstall
fi

echo "Building baan in release mode..."
cargo build --release

echo "Installing binary to $PREFIX/bin/baan..."
run_priv install -Dm755 target/release/baan "$PREFIX/bin/baan"

echo "Installing service unit..."
run_priv install -Dm644 baan@.service "$SERVICE_DIR/baan@.service"

echo "Installing config to $CONFIG_DIR/baan.toml..."
if [[ -z "$CONFIG_SRC" ]]; then
    CONFIG_SRC="baan.example.toml"
fi
if [[ ! -f "$CONFIG_SRC" ]]; then
    echo "Error: config source '$CONFIG_SRC' does not exist." >&2
    exit 1
fi
run_priv mkdir -p "$CONFIG_DIR"
run_priv install -m644 "$CONFIG_SRC" "$CONFIG_DIR/baan.toml"
echo "  (from $CONFIG_SRC)"

run_priv systemctl daemon-reload

if [[ -z "$KEYBOARD" ]]; then
    echo "Detecting keyboard device..."
    KEYBOARD=$(detect_keyboard)
fi
KEYBOARD=$(normalize_event "$KEYBOARD")
SERVICE="baan@$KEYBOARD.service"

echo "----------------------------------------"
echo "Installed:"
echo "  Binary:  $PREFIX/bin/baan"
echo "  Service: $SERVICE_DIR/baan@.service"
echo "  Config:  $CONFIG_DIR/baan.toml"
echo "----------------------------------------"

if [[ "$ENABLE_SERVICE" -eq 1 ]]; then
    echo "Enabling and starting $SERVICE..."
    run_priv systemctl enable --now "$SERVICE"
    echo
    echo "Status: sudo systemctl status $SERVICE"
    echo "Logs:   journalctl -u $SERVICE -f"
else
    echo "Skipped enabling service (--no-start). Enable manually with:"
    echo "  sudo systemctl enable --now $SERVICE"
fi
