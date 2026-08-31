mod clipboard;
mod command;
mod config;
mod device;
mod engine;
mod error;
mod event;
mod keycode;
mod singal;
mod tag;

use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info};

use config::{Settings, TriggerCommands, default_config_path};
use device::{KeyboardDevice, VirtualDevice};
use engine::run_keyboard_event_loop;
use error::{BaanError, Result};

use crate::{config::load_config, singal::install_signal_handlers};

#[derive(Parser, Debug)]
#[command(name = "baan", version, about = "Keyboard input expansion daemon")]
struct Args {
    /// Path to the keyboard device
    #[arg(short, long, env = "BAAN_KEYBOARD_PATH")]
    keyboard_path: PathBuf,

    /// Path to the config file
    #[arg(short, long, env = "BAAN_CONFIG_FILE_PATH", default_value_os_t = default_config_path())]
    config_path: PathBuf,
}

fn main() -> Result<()> {
    // Initialize tracing with env-filter for log levels.
    // Set RUST_LOG=debug for verbose output, or leave unset for info-level.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_thread_ids(true)
        .init();

    let args = Args::parse();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        keyboard = %args.keyboard_path.display(),
        config = %args.config_path.display(),
        "baan starting"
    );

    if unsafe { libc::geteuid() } != 0 {
        error!("Need sudo privileges");
        return Err(BaanError::Privileges);
    }

    // Configure the display environment before any threads are spawned.
    // `env::set_var` is unsafe in the 2024 edition once threads exist, so it
    // must happen up front (see clipboard.rs).
    if clipboard::ensure_display_environment().is_none() {
        error!("Failed to configure display environment");
        return Err(BaanError::Clipboard);
    }

    // Install signal handlers for graceful shutdown (no SA_RESTART so
    // blocking reads return EINTR and the event loop can exit promptly).
    install_signal_handlers()?;

    let (settings, trigger_commands) = load_config(&args.config_path)?;
    info!(
        config = %args.config_path.display(),
        triggers = trigger_commands.len(),
        flush_delay_ms = settings.flush_delay_ms,
        clipboard_read_delay_ms = settings.clipboard_read_delay_ms,
        clipboard_write_delay_ms = settings.clipboard_write_delay_ms,
        "Loaded configuration"
    );

    let result = run(args.keyboard_path, &settings, &trigger_commands);
    if let Err(e) = &result {
        error!(detail = %e, "Fatal error");
    }
    result
}

fn run(
    keyboard_path: PathBuf,
    settings: &Settings,
    trigger_commands: &TriggerCommands,
) -> Result<()> {
    let keyboard = KeyboardDevice::open(keyboard_path)?;
    let virtual_dev = VirtualDevice::new()?;
    let clipboard = clipboard::open().ok_or_else(|| {
        error!("Failed to initialize clipboard");
        BaanError::Clipboard
    })?;
    run_keyboard_event_loop(clipboard, keyboard, virtual_dev, trigger_commands, settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn args(extra: &[&str]) -> Vec<String> {
        std::iter::once("baan")
            .chain(extra.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parses_keyboard_and_config_paths() {
        let a =
            Args::try_parse_from(args(&["-k", "/dev/input/event0", "-c", "/tmp/x.toml"])).unwrap();
        assert_eq!(a.keyboard_path, PathBuf::from("/dev/input/event0"));
        assert_eq!(a.config_path, PathBuf::from("/tmp/x.toml"));
    }

    #[test]
    fn keyboard_path_is_required() {
        match Args::try_parse_from(args(&["-c", "/tmp/x.toml"])) {
            // If the ambient environment exports the path, the flag walks back
            // to being optional — accept that and declare the test moot.
            Ok(_) if std::env::var_os("BAAN_KEYBOARD_PATH").is_some() => {}
            Ok(_) => panic!("keyboard-path unexpectedly optional"),
            Err(_) => {}
        }
    }

    #[test]
    fn config_path_defaults_when_omitted() {
        let a = Args::try_parse_from(args(&["-k", "/dev/input/event0"])).unwrap();
        assert_eq!(a.config_path, default_config_path());
    }

    /// Env is process-global, so mutating it here would race with tests that
    /// run in parallel. Instead, exercise the env fallback in a spawned child
    /// that runs just this test with `BAAN_KEYBOARD_PATH` set; the child
    /// records the parsed path to a temp file for the parent to assert on.
    #[test]
    fn keyboard_path_reads_from_environment() {
        if std::env::var("BAAN_KEYBOARD_PATH_CHILD").is_ok() {
            let out_file = std::env::var("BAAN_KEYBOARD_PATH_OUT").expect("missing out file");
            let a = Args::try_parse_from(vec![
                "baan".to_string(),
                "-c".to_string(),
                "/tmp/x.toml".to_string(),
            ])
            .unwrap();
            std::fs::write(&out_file, a.keyboard_path.to_str().unwrap()).unwrap();
            std::process::exit(0);
        }

        let out_file = tempfile::NamedTempFile::new().unwrap();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .arg("keyboard_path_reads_from_environment")
            .env("BAAN_KEYBOARD_PATH", "/dev/input/event9")
            .env("BAAN_KEYBOARD_PATH_CHILD", "1")
            .env("BAAN_KEYBOARD_PATH_OUT", out_file.path())
            .output()
            .expect("failed to spawn child test process");
        assert!(
            out.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let parsed = std::fs::read_to_string(out_file.path()).unwrap();
        assert_eq!(parsed, "/dev/input/event9");
    }
}
