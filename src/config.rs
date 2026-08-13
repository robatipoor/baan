use tracing::warn;

use crate::error::{BaanError, Result};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use std::{collections::HashMap, path::Path};

pub const CONFIG_FILE_NAME: &str = "baan.toml";

/// The reserved TOML table that holds runtime settings.
///
/// Reserved: a trigger cannot be named `baan` at the root level, since that
/// key is always interpreted as the settings table.
const SETTINGS_TABLE: &str = "baan";

/// The TOML table that holds trigger definitions.
///
/// Reserved: a trigger cannot be named `triggers` at the root level, since
/// that key is always interpreted as the triggers table.
const TRIGGERS_TABLE: &str = "triggers";

/// Map of trigger → command arguments parsed from the config file.
pub type TriggerCommands = HashMap<String, Vec<String>>;

/// Runtime settings parsed from the `[baan]` section of the config file.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Delay before injecting replacement output (milliseconds).
    pub flush_delay_ms: u64,
    /// Delay before reading clipboard after Ctrl+C (milliseconds).
    pub clipboard_read_delay_ms: u64,
    /// Delay between writing to the clipboard and pasting, and before
    /// restoring the old clipboard value (milliseconds).
    pub clipboard_write_delay_ms: u64,
    /// Maximum time a trigger command may run before it is killed.
    pub command_timeout: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            flush_delay_ms: 100,
            clipboard_read_delay_ms: 120,
            clipboard_write_delay_ms: 80,
            command_timeout: Duration::from_secs(15),
        }
    }
}

/// Returns the default config path following the XDG Base Directory
/// specification: `$XDG_CONFIG_HOME/baan/baan.toml`, falling back to
/// `~/.config/baan/baan.toml` when `XDG_CONFIG_HOME` is unset. As a last
/// resort (`HOME` unset too) it returns `/baan.toml` with a warning.
pub fn default_config_path() -> PathBuf {
    let relative = PathBuf::from("baan").join(CONFIG_FILE_NAME);

    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);

    let dir = xdg.or_else(|| std::env::home_dir().map(|h| h.join(".config")));

    match dir {
        Some(dir) => dir.join(relative),
        None => {
            warn!(
                "Could not determine config directory; falling back to /{}",
                CONFIG_FILE_NAME
            );
            PathBuf::from("/").join(CONFIG_FILE_NAME)
        }
    }
}

pub fn read_config_file(config_path: &Path) -> Result<String> {
    fs::read_to_string(config_path).map_err(|e| BaanError::Open {
        path: config_path.display().to_string(),
        source: e,
    })
}

pub fn parse_config_file(content: &str) -> Result<(Settings, TriggerCommands)> {
    // Parse the entire file as a TOML table.
    let table: toml::Table = toml::from_str(content).map_err(|e| {
        let line = e
            .span()
            .map(|span| line_number_at(content, span.start))
            .unwrap_or(0);
        BaanError::ParseConfigFile {
            line,
            detail: format!("TOML parse error: {}", e),
        }
    })?;

    let mut settings = Settings::default();
    let mut trigger_commands = HashMap::new();

    for (key, value) in table {
        match key.as_str() {
            SETTINGS_TABLE => parse_settings(&value, &mut settings)?,
            TRIGGERS_TABLE => parse_triggers(value, &mut trigger_commands)?,
            _ => parse_trigger(key, value, &mut trigger_commands)?,
        }
    }

    Ok((settings, trigger_commands))
}

/// Converts a byte offset into a 1-indexed line number by counting
/// newlines before it.
fn line_number_at(content: &str, byte_offset: usize) -> usize {
    let end = byte_offset.min(content.len());
    content[..end].matches('\n').count() + 1
}

/// Parse the contents of the `[triggers]` table.
fn parse_triggers(value: toml::Value, map: &mut TriggerCommands) -> Result<()> {
    let table = match value {
        toml::Value::Table(t) => t,
        _ => {
            return Err(BaanError::ParseConfigFile {
                line: 0,
                detail: format!("[{TRIGGERS_TABLE}] must be a table"),
            });
        }
    };

    for (key, value) in table {
        parse_trigger(key, value, map)?;
    }

    Ok(())
}

/// Parse a single trigger definition (key → command array).
///
/// Also handles the legacy root-level keys for backward compatibility.
fn parse_trigger(key: String, value: toml::Value, map: &mut TriggerCommands) -> Result<()> {
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(BaanError::ParseConfigFile {
            line: 0,
            detail: "Empty trigger key is not allowed".to_string(),
        });
    }

    let values: Vec<String> = match value {
        toml::Value::Array(arr) => arr
            .into_iter()
            .map(|v| value_to_arg(&key, v))
            .collect::<Result<_>>()?,
        other => vec![value_to_arg(&key, other)?],
    };

    if map.contains_key(&key) {
        return Err(BaanError::ParseConfigFile {
            line: 0,
            detail: format!("Duplicate trigger '{}'", key),
        });
    }

    map.insert(key, values);
    Ok(())
}

/// Converts a single TOML value into a command-line argument string.
///
/// Scalars (strings, integers, floats, booleans, datetimes) are converted
/// to their plain text representation. Arrays and tables are rejected with
/// an explicit error rather than being silently stringified into a single
/// argument containing raw TOML syntax (e.g. `["a", "b"]`), which is almost
/// never what's intended and previously happened silently.
fn value_to_arg(trigger: &str, value: toml::Value) -> Result<String> {
    match value {
        toml::Value::String(s) => Ok(s),
        toml::Value::Integer(i) => Ok(i.to_string()),
        toml::Value::Float(f) => Ok(f.to_string()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        toml::Value::Datetime(d) => Ok(d.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => Err(BaanError::ParseConfigFile {
            line: 0,
            detail: format!(
                "trigger '{trigger}' has a command argument that is an array or table; \
                 only strings, numbers, booleans, and datetimes are allowed"
            ),
        }),
    }
}

fn parse_settings(value: &toml::Value, settings: &mut Settings) -> Result<()> {
    let table = match value {
        toml::Value::Table(t) => t,
        _ => {
            return Err(BaanError::ParseConfigFile {
                line: 0,
                detail: format!("[{SETTINGS_TABLE}] must be a table"),
            });
        }
    };

    for key in table.keys().filter(|k| {
        !matches!(
            k.as_str(),
            "flush_delay_ms"
                | "clipboard_read_delay_ms"
                | "clipboard_write_delay_ms"
                | "command_timeout_secs"
        )
    }) {
        warn!(key = %key, "Unknown setting in [baan] table, ignoring");
    }

    if let Some(v) = table.get("flush_delay_ms") {
        settings.flush_delay_ms = parse_positive_u64(v, "flush_delay_ms")?;
    }
    if let Some(v) = table.get("clipboard_read_delay_ms") {
        settings.clipboard_read_delay_ms = parse_positive_u64(v, "clipboard_read_delay_ms")?;
    }
    if let Some(v) = table.get("clipboard_write_delay_ms") {
        settings.clipboard_write_delay_ms = parse_positive_u64(v, "clipboard_write_delay_ms")?;
    }
    if let Some(v) = table.get("command_timeout_secs") {
        if let Some(secs) = v.as_integer().filter(|&s| s > 0) {
            settings.command_timeout = Duration::from_secs(secs as u64);
        } else {
            return Err(BaanError::ParseConfigFile {
                line: 0,
                detail: "command_timeout_secs must be a positive integer".to_string(),
            });
        }
    }

    Ok(())
}

fn parse_positive_u64(value: &toml::Value, name: &str) -> Result<u64> {
    let as_int = value
        .as_integer()
        .ok_or_else(|| BaanError::ParseConfigFile {
            line: 0,
            detail: format!("{name} must be an integer"),
        })?;

    as_int.try_into().map_err(|_| BaanError::ParseConfigFile {
        line: 0,
        detail: format!("{name} must be a non-negative integer"),
    })
}

pub fn load_config(config_path: &Path) -> Result<(Settings, TriggerCommands)> {
    let content = read_config_file(config_path)?;
    parse_config_file(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ... all existing tests unchanged ...

    // ---- default_config_path: XDG resolution --------------------------

    /// Runs the calling test in a child process with the given env overrides so
    /// the process-global environment is never mutated while other tests run in
    /// parallel. The child writes the resolved path to a temp file; the parent
    /// returns it.
    fn resolve_in_child(test_name: &str, env_pairs: &[(&str, &str)]) -> PathBuf {
        if std::env::var("BAAN_PATH_CHILD").is_ok() {
            let out_file = std::env::var("BAAN_PATH_OUT_FILE").expect("missing out file");
            std::fs::write(&out_file, super::default_config_path().to_str().unwrap()).unwrap();
            std::process::exit(0);
        }
        let out_file = tempfile::NamedTempFile::new().unwrap();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .arg(test_name)
            .env("BAAN_PATH_CHILD", "1")
            .env("BAAN_PATH_OUT_FILE", out_file.path())
            .envs(env_pairs.iter().copied())
            .output()
            .expect("failed to spawn child test process");
        assert!(
            out.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        PathBuf::from(std::fs::read_to_string(out_file.path()).unwrap())
    }

    #[test]
    fn default_config_path_prefers_xdg_config_home() {
        let path = resolve_in_child(
            "default_config_path_prefers_xdg_config_home",
            &[("XDG_CONFIG_HOME", "/tmp/custom-xdg")],
        );
        assert_eq!(path, PathBuf::from("/tmp/custom-xdg/baan/baan.toml"));
    }

    #[test]
    fn default_config_path_falls_back_to_home_config() {
        let path = resolve_in_child(
            "default_config_path_falls_back_to_home_config",
            &[
                ("XDG_CONFIG_HOME", ""), // unset → ignored
                ("HOME", "/home/tester"),
            ],
        );
        assert_eq!(path, PathBuf::from("/home/tester/.config/baan/baan.toml"));
    }

    #[test]
    fn default_config_path_ignores_empty_xdg_and_home() {
        // An empty `HOME` is treated as unset; home_dir() falls back to the
        // passwd database, so the config still resolves under that home.
        let path = resolve_in_child(
            "default_config_path_ignores_empty_xdg_and_home",
            &[("XDG_CONFIG_HOME", ""), ("HOME", "")],
        );
        let path_str = path.to_string_lossy().into_owned();
        assert!(
            path_str.ends_with(".config/baan/baan.toml"),
            "got {path_str}"
        );
        assert!(
            path_str.starts_with('/'),
            "expected absolute path, got {path_str}"
        );
    }

    // ... existing tests below ...

    #[test]
    fn nested_array_in_trigger_errors() {
        let content = r#"cmd = ["echo", ["a", "b"]]"#;
        let err = parse_config_file(content).unwrap_err();
        match err {
            BaanError::ParseConfigFile { detail, .. } => {
                assert!(detail.contains("array or table"));
                assert!(detail.contains("cmd"));
            }
            _ => panic!("Expected ParseConfigFile error"),
        }
    }

    #[test]
    fn nested_table_in_trigger_errors() {
        let content = r#"cmd = ["echo", { a = 1 }]"#;
        let err = parse_config_file(content).unwrap_err();
        match err {
            BaanError::ParseConfigFile { detail, .. } => {
                assert!(detail.contains("array or table"));
            }
            _ => panic!("Expected ParseConfigFile error"),
        }
    }

    #[test]
    fn integer_and_bool_array_elements_stringified() {
        // Unquoted scalars are valid TOML and should convert cleanly,
        // without going through Display-formatted TOML syntax.
        let content = r#"cmd = ["echo", 42, true, 3.5]"#;
        let (_, map) = parse_config_file(content).unwrap();
        assert_eq!(map.get("cmd").unwrap(), &vec!["echo", "42", "true", "3.5"]);
    }

    #[test]
    fn default_settings_timeout_is_15_secs() {
        let (settings, _) = parse_config_file("[baan]\n").unwrap();
        assert_eq!(settings.command_timeout, Duration::from_secs(15));
    }

    #[test]
    fn command_timeout_secs_parsed_from_baan_table() {
        let (settings, _) = parse_config_file("[baan]\ncommand_timeout_secs = 30\n").unwrap();
        assert_eq!(settings.command_timeout, Duration::from_secs(30));
    }

    #[test]
    fn command_timeout_secs_rejects_non_positive_integer() {
        for bad in [
            "command_timeout_secs = 0",
            "command_timeout_secs = -1",
            "command_timeout_secs = \"30\"",
        ] {
            let err = parse_config_file(&format!("[baan]\n{bad}")).unwrap_err();
            match err {
                BaanError::ParseConfigFile { detail, .. } => {
                    assert!(detail.contains("command_timeout_secs"), "got {detail}");
                }
                _ => panic!("Expected ParseConfigFile error"),
            }
        }
    }

    #[test]
    fn parse_error_reports_line_number() {
        let content = "greet = [\"hi\"\ncmd = [\"missing bracket\"\n";
        let err = parse_config_file(content).unwrap_err();
        match err {
            BaanError::ParseConfigFile { line, .. } => {
                assert!(line > 0, "expected a non-zero line number, got {line}");
            }
            _ => panic!("Expected ParseConfigFile error"),
        }
    }
}
