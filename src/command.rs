use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use tracing::{debug, error, warn};
use wait_timeout::ChildExt;

use crate::error::{BaanError, Result};

/// Default command timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 15;

/// Cap on captured stdout/stderr size (bytes) to avoid unbounded memory use.
const MAX_OUTPUT_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Whether `command` names a shell program.
fn is_shell_command(command: &str) -> bool {
    let name = Path::new(command)
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| command.to_string().into());
    matches!(
        name.as_ref(),
        "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" | "mksh"
    )
}

/// Index of the script argument for a `<shell> -c <script>` invocation, if any.
fn shell_script_index(args: &[String]) -> Option<usize> {
    args.iter().position(|a| a == "-c").map(|i| i + 1)
}

/// Whether any argument contains a `{}` placeholder.
pub fn has_placeholders(args: &[String]) -> bool {
    args.iter().any(|a| a.contains("{}"))
}

/// Removes invalid and problematic characters from a replacement value.
///
/// This strips NUL bytes (which break OS process spawning) as well as
/// newlines and quotes that might break shell scripts or downstream
/// applications if not handled perfectly.
fn sanitize_value(value: &str) -> String {
    value
        .chars()
        .filter(|&c| !matches!(c, '\0' | '\n' | '\r' | '"' | '\''))
        .collect()
}

/// Tracks which shell quoting context the scanner is currently inside.
#[derive(Clone, Copy, PartialEq)]
enum QuoteState {
    Unquoted,
    Single,
    Double,
}

/// Rewrite every `{}` in a shell `-c` script argument into a positional
/// parameter reference (`$1`, `$2`, ...), tracking quote state so the
/// reference is always inserted somewhere it will actually expand.
fn substitute_shell_placeholders(
    script: &str,
    value_options: &[&str],
    used: &mut usize,
) -> Result<(String, Vec<String>)> {
    let mut state = QuoteState::Unquoted;
    let mut out = String::with_capacity(script.len());
    let mut values: Vec<String> = Vec::new();
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;

    let next_value = |used: &mut usize| -> Result<String> {
        let value = *value_options.get(*used).ok_or_else(|| BaanError::Command {
            detail: format!(
                "Not enough replacement values: expected at least {}, got {}",
                *used + 1,
                value_options.len()
            ),
        })?;
        *used += 1;
        let value = sanitize_value(value);
        Ok(value)
    };

    while i < chars.len() {
        let c = chars[i];
        let is_placeholder = c == '{' && chars.get(i + 1) == Some(&'}');

        match state {
            QuoteState::Single => {
                if is_placeholder {
                    let value = next_value(used)?;
                    values.push(value);
                    // Close the currently-open single quote, insert a
                    // safely double-quoted expansion, then reopen a single
                    // quote to continue the rest of the literal text.
                    out.push_str(&format!("'\"${}\"'", values.len()));
                    i += 2;
                } else if c == '\'' {
                    state = QuoteState::Unquoted;
                    out.push(c);
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            QuoteState::Double => {
                if is_placeholder {
                    let value = next_value(used)?;
                    values.push(value);
                    // Already inside double quotes: $N expands here
                    // without any further quoting gymnastics needed.
                    out.push_str(&format!("${}", values.len()));
                    i += 2;
                } else if c == '\\' && i + 1 < chars.len() {
                    out.push(c);
                    out.push(chars[i + 1]);
                    i += 2;
                } else if c == '"' {
                    state = QuoteState::Unquoted;
                    out.push(c);
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            QuoteState::Unquoted => {
                if is_placeholder {
                    let value = next_value(used)?;
                    values.push(value);
                    // Bare: wrap our own reference in double quotes so the
                    // value can't be word-split or glob-expanded.
                    out.push_str(&format!("\"${}\"", values.len()));
                    i += 2;
                } else if c == '\\' && i + 1 < chars.len() {
                    out.push(c);
                    out.push(chars[i + 1]);
                    i += 2;
                } else if c == '\'' {
                    state = QuoteState::Single;
                    out.push(c);
                    i += 1;
                } else if c == '"' {
                    state = QuoteState::Double;
                    out.push(c);
                    i += 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }

    Ok((out, values))
}

/// Replace `{}` in every argument with the next replacement value.
pub fn expand_placeholders(
    command: &str,
    options: &[String],
    value_options: &[&str],
) -> Result<Vec<String>> {
    let is_shell = is_shell_command(command);
    let script_idx = is_shell.then(|| shell_script_index(options)).flatten();

    let mut used = 0usize;
    let mut out = Vec::with_capacity(options.len() + 1);
    let mut trailing_positional: Vec<String> = Vec::new();

    out.push(command.to_string());

    for (i, arg) in options.iter().enumerate() {
        if Some(i) == script_idx {
            if arg.contains("{}") {
                let (rewritten, values) =
                    substitute_shell_placeholders(arg, value_options, &mut used)?;
                trailing_positional = values;
                out.push(rewritten);
            } else {
                out.push(arg.to_string());
            }
            continue;
        }

        if !arg.contains("{}") {
            out.push(arg.to_string());
            continue;
        }

        let mut result = String::with_capacity(arg.len());
        let mut rest = arg.as_str();
        while let Some(pos) = rest.find("{}") {
            result.push_str(&rest[..pos]);
            let value = value_options.get(used).ok_or_else(|| BaanError::Command {
                detail: format!(
                    "Not enough replacement values: expected at least {}, got {}",
                    used + 1,
                    value_options.len()
                ),
            })?;
            used += 1;
            let value = sanitize_value(value);
            result.push_str(&value);
            rest = &rest[pos + 2..];
        }
        result.push_str(rest);
        out.push(result);
    }

    if used != value_options.len() {
        return Err(BaanError::Command {
            detail: format!(
                "{} replacement value(s) provided but only {} placeholder(s) found",
                value_options.len(),
                used
            ),
        });
    }

    if !trailing_positional.is_empty() {
        if let Some(idx) = script_idx
            && out.len() == idx + 2
        {
            out.push("baan".to_string());
        }
        out.extend(trailing_positional);
    }

    Ok(out)
}

/// Run a command with its arguments and a timeout.
pub fn run_command(command: &str, args: &[String], timeout: Duration) -> Result<String> {
    debug!(command, ?args, "Running command");

    if command.is_empty() {
        let msg = "Command name is empty".to_string();
        error!(detail = %msg);
        return Err(BaanError::Command { detail: msg });
    }

    let mut child = Command::new(command)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            let msg = format!("Failed to run command '{}': {}", command, e);
            error!(command, detail = %msg, "Command spawn error");
            BaanError::Command { detail: msg }
        })?;

    let stdout_handle = read_stream(child.stdout.take());
    let stderr_handle = read_stream(child.stderr.take());

    let status = match child.wait_timeout(timeout).map_err(|e| {
        let msg = format!("Failed to wait on command: {}", e);
        error!(command, detail = %msg);
        BaanError::Command { detail: msg }
    })? {
        Some(status) => status,
        None => {
            warn!(
                command,
                timeout_secs = timeout.as_secs(),
                "Command timed out, killing process"
            );
            if let Err(e) = child.kill() {
                warn!(command, error = %e, "Failed to kill timed-out process");
            }
            let _ = child.wait();
            join_readers(stdout_handle, stderr_handle)?;
            return Err(BaanError::Command {
                detail: format!(
                    "Command '{}' timed out after {}s",
                    command,
                    timeout.as_secs()
                ),
            });
        }
    };

    let (stdout, stderr) = join_readers(stdout_handle, stderr_handle)?;

    if !status.success() {
        let msg = format!(
            "Command '{}' exited with {}: {}",
            command,
            status,
            stderr.trim()
        );
        error!(command, detail = %msg, "Command failed");
        return Err(BaanError::Command { detail: msg });
    }

    Ok(stdout)
}

/// Convenience wrapper using the default timeout.
pub fn run_command_default(command: &str, args: &[String]) -> Result<String> {
    run_command(command, args, Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

/// Read a child pipe to EOF (capped) on a background thread.
///
/// After reaching `MAX_OUTPUT_BYTES`, the reader **continues to drain**
/// the pipe (discarding excess data) so the child process never blocks
/// on a full pipe buffer. Without this, a child that produces more than
/// the cap would deadlock: it couldn't write, so it couldn't exit, and
/// `wait_timeout` would hang until the timeout.
fn read_stream<R: Read + Send + 'static>(
    stream: Option<R>,
) -> thread::JoinHandle<std::io::Result<String>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stream {
            let mut chunk = [0u8; 8192];
            loop {
                let n = match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                if buf.len() < MAX_OUTPUT_BYTES {
                    let remaining = MAX_OUTPUT_BYTES.saturating_sub(buf.len());
                    let to_copy = n.min(remaining);
                    buf.extend_from_slice(&chunk[..to_copy]);
                }
                // Keep draining even past the cap so the child doesn't
                // block on a full pipe.
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })
}

fn join_readers(
    stdout_handle: thread::JoinHandle<std::io::Result<String>>,
    stderr_handle: thread::JoinHandle<std::io::Result<String>>,
) -> Result<(String, String)> {
    let stdout = stdout_handle
        .join()
        .map_err(|_| BaanError::Command {
            detail: "stdout reader thread panicked".to_string(),
        })?
        .map_err(|e| {
            let msg = format!("Failed to read command output: {}", e);
            error!(detail = %msg);
            BaanError::Command { detail: msg }
        })?;
    let stderr = stderr_handle
        .join()
        .map_err(|_| BaanError::Command {
            detail: "stderr reader thread panicked".to_string(),
        })?
        .map_err(|e| {
            let msg = format!("Failed to read command stderr: {}", e);
            error!(detail = %msg);
            BaanError::Command { detail: msg }
        })?;
    Ok((stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ---- expand_placeholders: shell script argument ------------------------

    #[test]
    fn expand_placeholders_bare_becomes_quoted_positional_param() {
        let args = ["sh".to_string(), "-c".to_string(), "echo {}".to_string()];
        let result = expand_placeholders(&args[0], &args[1..], &["hello world"]).unwrap();
        assert_eq!(result[2], "echo \"$1\"");
        assert_eq!(result[3], "baan");
        assert_eq!(result[4], "hello world");
    }

    #[test]
    fn expand_placeholders_double_quoted_placeholder_uses_bare_dollar() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            "echo \"{}\"".to_string(),
        ];
        let result = expand_placeholders(&args[0], &args[1..], &["$(rm -rf /)"]).unwrap();
        assert_eq!(result[2], "echo \"$1\"");
        assert_eq!(result[4], "$(rm -rf /)");
    }

    #[test]
    fn expand_placeholders_json_style_single_quoted_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"curl localhost:8008 -d '{"first_name":"{}"}'"#.to_string(),
        ];
        let raw = "Robert'); DROP TABLE users;--";
        let sanitized = sanitize_value(raw);
        let result = expand_placeholders(&args[0], &args[1..], &[raw]).unwrap();
        assert_eq!(
            result[2],
            r#"curl localhost:8008 -d '{"first_name":"'"$1"'"}'"#
        );
        // The single quote is stripped by sanitize_value
        assert_eq!(result[4], sanitized);
        assert_eq!(result[4], "Robert); DROP TABLE users;--");
    }

    #[test]
    fn expand_placeholders_multiple_placeholders_distinct_values() {
        let args = [
            "bash".to_string(),
            "-c".to_string(),
            "echo {} | grep {}".to_string(),
        ];
        let result =
            expand_placeholders(&args[0], &args[1..], &["test; rm -rf /", "needle"]).unwrap();
        assert_eq!(result[2], "echo \"$1\" | grep \"$2\"");
        assert_eq!(result[3], "baan");
        assert_eq!(result[4], "test; rm -rf /");
        assert_eq!(result[5], "needle");
    }

    #[test]
    fn expand_placeholders_reuses_templates_own_dollar_zero() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            "echo {}".to_string(),
            "plain arg".to_string(),
        ];
        let result = expand_placeholders(&args[0], &args[1..], &["$(rm -rf /)"]).unwrap();
        assert_eq!(result[2], "echo \"$1\"");
        assert_eq!(result[3], "plain arg");
        assert_eq!(result[4], "$(rm -rf /)");
    }

    #[test]
    fn expand_placeholders_no_escape_for_direct_command() {
        let args = ["echo".to_string(), "{}".to_string()];
        let result = expand_placeholders(&args[0], &args[1..], &["hello world"]).unwrap();
        assert_eq!(result[1], "hello world");
    }

    #[test]
    fn expand_placeholders_no_escape_for_direct_command_full_path() {
        let args = ["/usr/bin/echo".to_string(), "{}".to_string()];
        let result = expand_placeholders(&args[0], &args[1..], &["hello world"]).unwrap();
        assert_eq!(result[1], "hello world");
    }

    #[test]
    fn expand_placeholders_fewer_value_options_errors() {
        let args = ["sh".to_string(), "-c".to_string(), "echo {} {}".to_string()];
        let result = expand_placeholders(&args[0], &args[1..], &["a"]);
        assert!(result.is_err());
    }

    #[test]
    fn expand_placeholders_more_value_options_errors() {
        let args = ["echo".to_string(), "{}".to_string()];
        let result = expand_placeholders(&args[0], &args[1..], &["a", "b"]);
        assert!(result.is_err());
    }

    #[test]
    fn expand_placeholders_no_placeholders_no_value_options_ok() {
        let args = vec!["echo".to_string(), "hello".to_string()];
        let result = expand_placeholders(&args[0], &args[1..], &[]).unwrap();
        assert_eq!(result, args);
    }

    #[test]
    fn has_placeholders_detects_any_occurrence() {
        assert!(has_placeholders(&["echo".to_string(), "{}".to_string()]));
        assert!(has_placeholders(&[
            "sh".to_string(),
            "-c".to_string(),
            "echo {}".to_string()
        ]));
        assert!(!has_placeholders(&[
            "date".to_string(),
            "+%Y%m%d".to_string()
        ]));
    }

    // ---- security regression tests: actually execute the command ----------

    #[test]
    fn run_command_bare_placeholder_value_is_never_executed_as_shell_code() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' {}".to_string(),
        ];
        let marker = "/tmp/baan_test_pwned_marker_bare";
        let _ = std::fs::remove_file(marker);
        let raw = format!("$(touch {marker}); `id`; a'b\"c");
        let sanitized = sanitize_value(&raw);

        let expanded = expand_placeholders(&args[0], &args[1..], &[&raw]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();

        assert_eq!(
            result, sanitized,
            "value must come back exactly as sanitized"
        );
        assert!(!Path::new(marker).exists(), "value must never be executed");
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn run_command_json_style_placeholder_prevents_injection() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{"first_name":"{}"}'"#.to_string(),
        ];
        let marker = "/tmp/baan_test_pwned_marker_json";
        let _ = std::fs::remove_file(marker);
        let raw = format!("x\"}}'; touch {marker}; echo '");
        let sanitized = sanitize_value(&raw);

        let expanded = expand_placeholders(&args[0], &args[1..], &[&raw]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();

        assert_eq!(result, format!(r#"{{"first_name":"{}"}}"#, sanitized));
        assert!(!Path::new(marker).exists(), "value must never be executed");
        let _ = std::fs::remove_file(marker);
    }

    // ---- NEW: tests for newlines, double quotes, and mixed special chars ---

    #[test]
    fn run_command_value_with_newline_bare_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' {}".to_string(),
        ];
        let value = "line1\nline2\nline3";
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_newline_double_quoted_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' "{}""#.to_string(),
        ];
        let value = "line1\nline2";
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_newline_single_quoted_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{}'"#.to_string(),
        ];
        let value = "line1\nline2";
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_double_quotes_bare_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' {}".to_string(),
        ];
        let value = r#"hello "world" "test""#;
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_double_quotes_double_quoted_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' "{}""#.to_string(),
        ];
        let value = r#"a "b" "c" d"#;
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_double_quotes_single_quoted_placeholder() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{}'"#.to_string(),
        ];
        let value = r#"hello "world""#;
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_mixed_special_chars() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            "printf '%s' {}".to_string(),
        ];
        let value = "line1\nline2\"quotes\" 'single' $HOME `whoami` \\backslash; semicolon";
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_value_with_newline_and_double_quote_in_json() {
        let args = [
            "sh".to_string(),
            "-c".to_string(),
            r#"printf '%s' '{"msg":"{}"}'"#.to_string(),
        ];
        let value = "hello\nworld\"escaped";
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, format!(r#"{{"msg":"{}"}}"#, sanitized));
    }

    #[test]
    fn run_command_non_shell_value_with_newline() {
        let args = ["printf".to_string(), "%s".to_string(), "{}".to_string()];
        let value = "line1\nline2";
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    #[test]
    fn run_command_non_shell_value_with_double_quotes() {
        let args = ["printf".to_string(), "%s".to_string(), "{}".to_string()];
        let value = r#"hello "world" test"#;
        let sanitized = sanitize_value(value);
        let expanded = expand_placeholders(&args[0], &args[1..], &[value]).unwrap();
        let result = run_command(&expanded[0], &expanded[1..], Duration::from_secs(5)).unwrap();
        assert_eq!(result, sanitized);
    }

    // ---- run_command --------------------------------------------------------

    #[test]
    fn run_command_empty_command_returns_error() {
        assert!(run_command("", &[], Duration::from_secs(5)).is_err());
    }

    #[test]
    fn run_command_large_output_does_not_deadlock() {
        let script = "i=0; while [ $i -lt 50000 ]; do echo 01234567890123456789; i=$((i+1)); done";
        let result = run_command(
            "sh",
            &["-c".to_string(), script.to_string()],
            Duration::from_secs(10),
        );
        let out = result.unwrap();
        assert!(out.len() > 100_000);
    }

    #[test]
    fn run_command_timeout_kills_process() {
        let result = run_command(
            "sh",
            &["-c".to_string(), "sleep 5".to_string()],
            Duration::from_millis(200),
        );
        assert!(result.is_err());
    }

    #[test]
    fn run_command_default_uses_default_timeout() {
        let result = run_command_default("echo", &["hi".to_string()]);
        assert_eq!(result.unwrap().trim(), "hi");
    }
}
