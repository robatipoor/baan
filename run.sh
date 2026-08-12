#!/bin/bash
# ==========================================
# CONFIGURATION
# Change these defaults if needed, or override
# using -k and -c flags when running.
# ==========================================
DEFAULT_KEYBOARD_PATH="/dev/input/event3"
DEFAULT_CONFIG_PATH=""                    # Empty = let the Rust binary use its own default

# Initialize variables
KEYBOARD_PATH="$DEFAULT_KEYBOARD_PATH"
CONFIG_PATH="$DEFAULT_CONFIG_PATH"
MODE=""

# Function to display usage
usage() {
    echo "Usage: $0 [-d|-r] [-k PATH] [-c PATH]"
    echo ""
    echo "Options:"
    echo "  -d, --debug          Build and run in debug mode"
    echo "  -r, --release        Build and run in release mode"
    echo "  -k, --keyboard PATH  Override the keyboard device path"
    echo "  -c, --config PATH    Override the config file path"
    echo "  -h, --help           Show this help message"
    echo ""
    echo "Examples:"
    echo "  $0 -d                          # Debug build, default config (~/baan.toml)"
    echo "  $0 -r -k /dev/input/event4     # Release with custom keyboard"
    echo "  $0 -d -c ./baan.toml           # Debug build with local config file"
    echo "  $0 -d -k /dev/input/event5 -c ~/baan.toml"
    exit 1
}

# Parse command-line arguments
while [[ "$#" -gt 0 ]]; do
    case $1 in
        -d|--debug)
            MODE="debug"
            ;;
        -r|--release)
            MODE="release"
            ;;
        -k|--keyboard)
            if [[ -n "$2" && "$2" != -* ]]; then
                KEYBOARD_PATH="$2"
                shift
            else
                echo "Error: -k/--keyboard requires a path argument."
                exit 1
            fi
            ;;
        -c|--config)
            if [[ -n "$2" && "$2" != -* ]]; then
                CONFIG_PATH="$2"
                shift
            else
                echo "Error: -c/--config requires a path argument."
                exit 1
            fi
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Error: Unknown option '$1'"
            usage
            ;;
    esac
    shift
done

# Check if mode was provided
if [[ -z "$MODE" ]]; then
    echo "Error: You must specify a mode (-d or -r)."
    usage
fi

# Build the extra argument for config-path (only if provided)
CONFIG_ARG=""
if [[ -n "$CONFIG_PATH" ]]; then
    CONFIG_ARG="--config-path=$CONFIG_PATH"
fi

# Execute based on mode
if [[ "$MODE" == "debug" ]]; then
    echo "Building in debug mode..."
    if cargo build; then
        echo "Running in debug mode (Keyboard: $KEYBOARD_PATH)..."
        if [[ -n "$CONFIG_PATH" ]]; then
            echo "Using config file: $CONFIG_PATH"
        fi
        sudo -E ./target/debug/baan -k "$KEYBOARD_PATH" $CONFIG_ARG
    else
        echo "Error: Debug build failed!"
        exit 1
    fi

elif [[ "$MODE" == "release" ]]; then
    echo "Building in release mode..."
    if cargo build --release; then
        echo "Running in release mode (Keyboard: $KEYBOARD_PATH)..."
        if [[ -n "$CONFIG_PATH" ]]; then
            echo "Using config file: $CONFIG_PATH"
        fi
        sudo -E ./target/release/baan -k "$KEYBOARD_PATH" $CONFIG_ARG
    else
        echo "Error: Release build failed!"
        exit 1
    fi
fi
