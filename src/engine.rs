use crate::{
    clipboard::ClipboardOps,
    command::{expand_placeholders, has_placeholders, run_command},
    config::{Settings, TriggerCommands},
    device::{KeyInjector, KeyboardDevice, VirtualDevice},
    error::Result,
    event::InputEvent,
    keycode::{
        EV_KEY, KEY_BACKSPACE, KEY_C, KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_RIGHTCTRL, KEY_RIGHTSHIFT,
        get_char_from_keycode, is_supported_key_code,
    },
    singal::TERMINATE,
    tag::TagParser,
};
use arboard::Clipboard;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Delay between line-selection steps during injection (milliseconds).
const SELECT_STEP_DELAY_MS: u64 = 50;

/// Messages from the keyboard reader thread to the main event loop.
enum KeyboardMessage {
    /// A keyboard event was read successfully.
    Event(InputEvent),
    /// The keyboard read returned an error.
    Error(String),
}

/// Command results sent back from background threads to the event loop.
enum CommandMessage {
    /// A tag was expanded inside the current line; replace the tag text.
    ReplaceTag { tag_text: String, output: String },
    /// A clipboard command finished; put the output on the clipboard and paste.
    SetClipboard { output: String },
}

/// Reason a clipboard-triggered command couldn't be resolved into a
/// runnable argv, extracted so tests can check *why* without spawning
/// a thread or running a process.
#[derive(Debug, PartialEq, Eq)]
enum ClipboardCommandError {
    NoTag,
    UnknownTag { tag_name: String },
    ExpansionFailed { tag_name: String, detail: String },
}

/// Resolves `captured_text` into a runnable argv: parse the tag, look up its
/// trigger, and expand placeholders. Pure — no threads or process execution.
fn resolve_clipboard_command(
    trigger_commands: &TriggerCommands,
    captured_text: &str,
) -> std::result::Result<Vec<String>, ClipboardCommandError> {
    let (tag_name, arg) = parse_tag(captured_text, ":?").ok_or(ClipboardCommandError::NoTag)?;
    let command = match trigger_commands.get(tag_name.trim()) {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            return Err(ClipboardCommandError::UnknownTag {
                tag_name: tag_name.to_string(),
            });
        }
    };

    match arg {
        Some(a) => expand_placeholders(&command[0], &command[1..], &[a]).map_err(|e| {
            ClipboardCommandError::ExpansionFailed {
                tag_name: tag_name.to_string(),
                detail: e.to_string(),
            }
        }),
        None if has_placeholders(command) => Err(ClipboardCommandError::ExpansionFailed {
            tag_name: tag_name.to_string(),
            detail: "Command requires an argument but none was provided".to_string(),
        }),
        None => Ok(command.to_vec()),
    }
}

/// Expands `command`'s placeholders with `content` if present. Pure — no
/// threads or process execution.
fn resolve_tag_expansion(
    command: &str,
    options: &[String],
    content: Option<&str>,
) -> Result<Vec<String>> {
    match content {
        Some(c) => expand_placeholders(command, options, &[c]),
        None => Ok(std::iter::once(command.to_string())
            .chain(options.iter().cloned())
            .collect()),
    }
}

/// Splits `input` on the first `separator`, returning `(tag, arg)`.
/// Returns `None` when the tag part is empty; `arg` is `None` when the
/// separator is at the very end (e.g. `date:?`).
pub fn parse_tag<'a>(input: &'a str, separator: &str) -> Option<(&'a str, Option<&'a str>)> {
    let (tag, arg) = input.split_once(separator)?;
    if tag.is_empty() {
        return None;
    }
    let arg = (!arg.is_empty()).then_some(arg);
    Some((tag, arg))
}

/// Look up the command configured for `captured_text` (via `parse_tag`),
/// expand placeholders, and run it on a background thread. The output is sent
/// back as `CommandMessage::SetClipboard` so the event loop stays responsive.
fn spawn_clipboard_command(
    cmd_tx: mpsc::Sender<CommandMessage>,
    trigger_commands: &TriggerCommands,
    captured_text: &str,
    timeout: Duration,
) {
    let expanded = match resolve_clipboard_command(trigger_commands, captured_text) {
        Ok(expanded) => expanded,
        Err(ClipboardCommandError::NoTag) => {
            warn!("Clipboard text does not contain a valid tag");
            return;
        }
        Err(ClipboardCommandError::UnknownTag { tag_name }) => {
            warn!(tag = %tag_name, "No command configured for tag");
            return;
        }
        Err(ClipboardCommandError::ExpansionFailed { tag_name, detail }) => {
            warn!(tag = %tag_name, detail = %detail, "Failed to expand command placeholders");
            return;
        }
    };

    let tx = cmd_tx.clone();
    thread::spawn(
        move || match run_command(&expanded[0], &expanded[1..], timeout) {
            Ok(output) => {
                let _ = tx.send(CommandMessage::SetClipboard { output });
            }
            Err(err) => error!(detail = %err, "Command execution failed"),
        },
    );
}

/// Spawns the background execution of a matched tag's command, expanding
/// `content` into the command's placeholder (if present) first.
///
/// Placeholder-expansion failures are logged rather than silently dropped —
/// this is almost always a misconfigured `{}` count in `baan.toml`, and the
/// user should hear about it.
fn spawn_tag_command(
    cmd_tx: mpsc::Sender<CommandMessage>,
    tag_name: String,
    content: Option<String>,
    tag_text: String,
    command: String,
    options: Vec<String>,
    timeout: Duration,
) {
    thread::spawn(move || {
        let command = command;
        let options = options;
        let expanded = match resolve_tag_expansion(&command, options.as_slice(), content.as_deref())
        {
            Ok(expanded) => expanded,
            Err(err) => {
                error!(tag = %tag_name, "Failed to expand command placeholders error: {}",err);
                return;
            }
        };

        match run_command(&expanded[0], &expanded[1..], timeout) {
            Ok(output) => {
                debug!(tag = %tag_text, "Tag matched, output ready");
                let _ = cmd_tx.send(CommandMessage::ReplaceTag { tag_text, output });
            }
            Err(err) => {
                error!(tag = %tag_name, detail = %err, "Failed to execute command");
            }
        }
    });
}

/// Pastes `output` via the clipboard: sets clipboard text, waits for
/// ownership to register, then simulates Ctrl+V.
fn handle_set_clipboard<C, V>(
    output: String,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) where
    C: ClipboardOps,
    V: KeyInjector,
{
    let trimmed = output.trim_end().to_owned();
    if let Err(e) = clipboard.set_text(&trimmed) {
        error!(detail = %e, "Failed to set clipboard text");
        return;
    }

    // Wait for clipboard ownership to register before pasting.
    thread::sleep(Duration::from_millis(settings.clipboard_write_delay_ms));

    if let Err(e) = virtual_device.send_ctrl_v() {
        error!(detail = %e, "Failed to simulate Ctrl+V");
    }
}

/// Types `replacement` directly if it's ASCII, otherwise round-trips it
/// through the clipboard (for characters the virtual keyboard can't emit
/// directly) and pastes with Ctrl+V.
///
/// Returns whether the clipboard was used, so the caller knows whether to
/// wait before restoring the previous clipboard contents.
fn inject_replacement<C, V>(
    replacement: &str,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) -> Result<bool>
where
    C: ClipboardOps,
    V: KeyInjector,
{
    if replacement.is_ascii() {
        virtual_device.send_string(replacement)?;
        return Ok(false);
    }

    clipboard
        .set_text(replacement)
        .map_err(|e| crate::error::BaanError::Write {
            detail: "clipboard (non-ASCII replacement)".to_string(),
            source: std::io::Error::other(e),
        })?;

    // Wait for clipboard ownership to register before pasting.
    thread::sleep(Duration::from_millis(settings.clipboard_write_delay_ms));
    virtual_device.send_ctrl_v()?;
    Ok(true)
}

/// Replaces `tag_text` on the current line with `output`, by:
/// selecting and copying the line to locate the tag, moving the cursor to
/// it, deleting it, and typing/pasting the replacement — then restoring
/// whatever was on the clipboard beforehand.
fn handle_replace_tag<C, V>(
    tag_text: String,
    output: String,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) where
    C: ClipboardOps,
    V: KeyInjector,
{
    debug!(tag = %tag_text, "Command output received from background thread");
    thread::sleep(Duration::from_millis(settings.flush_delay_ms));

    // Save the clipboard so it can be restored; default to empty on failure.
    let old_clipboard = clipboard
        .get_text()
        .map(|s| s.to_string())
        .unwrap_or_default();

    // Select the entire current line and copy it.
    if let Err(e) = virtual_device.select_line() {
        error!(detail = %e, "Failed to select line");
        return;
    }
    thread::sleep(Duration::from_millis(SELECT_STEP_DELAY_MS));
    if let Err(e) = virtual_device.send_ctrl_c() {
        error!(detail = %e, "Failed to copy line");
        return;
    }
    thread::sleep(Duration::from_millis(settings.clipboard_read_delay_ms));

    // Read the line from clipboard to find the tag position.
    let line_text = match clipboard.get_text() {
        Ok(t) => t.to_string(),
        Err(e) => {
            error!(detail = %e, "Failed to read clipboard after copying line");
            return;
        }
    };

    // `find` returns a byte offset, but cursor movement is by character.
    let Some(pos) = line_text.find(&tag_text) else {
        warn!(tag = %tag_text, "Tag not found in current line");
        return;
    };
    let pos_chars = line_text[..pos].chars().count();
    let tag_len_chars = tag_text.chars().count();
    let replacement = output.trim_end();

    if let Err(e) = virtual_device.position_at_tag(pos_chars, tag_len_chars) {
        error!(detail = %e, "Failed to position cursor at tag");
        return;
    }

    let pasted_from_clipboard =
        match inject_replacement(replacement, clipboard, virtual_device, settings) {
            Ok(used_clipboard) => used_clipboard,
            Err(e) => {
                error!(detail = %e, "Failed to inject replacement text");
                return;
            }
        };

    // Restore the old clipboard. If the replacement was pasted from the
    // clipboard, wait so the app's paste request is served first.
    if pasted_from_clipboard {
        thread::sleep(Duration::from_millis(settings.clipboard_write_delay_ms));
    }
    if let Err(e) = clipboard.set_text(&old_clipboard) {
        error!(detail = %e, "Failed to set clipboard old value");
    }
}

/// Drains all currently-available command results and applies them.
fn drain_command_results<C, V>(
    cmd_rx: &mpsc::Receiver<CommandMessage>,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) where
    C: ClipboardOps,
    V: KeyInjector,
{
    while let Ok(msg) = cmd_rx.try_recv() {
        match msg {
            CommandMessage::SetClipboard { output } => {
                handle_set_clipboard(output, clipboard, virtual_device, settings);
            }
            CommandMessage::ReplaceTag { tag_text, output } => {
                handle_replace_tag(tag_text, output, clipboard, virtual_device, settings);
            }
        }
    }
}

/// Outcome of trying to fetch the next keyboard event.
enum NextEvent {
    /// A key event to process.
    Some(InputEvent),
    /// No event yet; caller should loop back around.
    None,
    /// The reader thread ended (error or disconnect); caller should stop.
    Stop,
}

fn next_keyboard_event(kb_rx: &mpsc::Receiver<KeyboardMessage>) -> NextEvent {
    match kb_rx.recv_timeout(Duration::from_millis(10)) {
        Ok(KeyboardMessage::Event(e)) => NextEvent::Some(e),
        Ok(KeyboardMessage::Error(e)) => {
            error!(detail = %e, "Keyboard read error");
            NextEvent::Stop
        }
        Err(mpsc::RecvTimeoutError::Timeout) => NextEvent::None,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            error!("Keyboard reader thread disconnected");
            NextEvent::Stop
        }
    }
}

/// Feeds `c` into the tag parser and, if a tag now matches a configured
/// trigger, spawns its command in the background.
fn feed_parser_char(
    parser: &mut TagParser,
    c: char,
    cmd_tx: &mpsc::Sender<CommandMessage>,
    trigger_commands: &TriggerCommands,
    timeout: Duration,
) {
    parser.consume(c);

    if let Some((tag_name, content, tag_text)) = parser.take() {
        let tag_name_trimmed = tag_name.trim();
        let command = match trigger_commands.get(tag_name_trimmed) {
            Some(c) if !c.is_empty() => c,
            _ => return,
        };

        let cmd = command[0].clone();
        let options = command[1..].to_vec();

        spawn_tag_command(
            cmd_tx.clone(),
            tag_name_trimmed.to_string(),
            content,
            tag_text,
            cmd,
            options,
            timeout,
        );
    }
}

pub fn process_keyboard_events(
    mut clipboard: Clipboard,
    mut keyboard_device: KeyboardDevice,
    mut virtual_device: VirtualDevice,
    trigger_commands: &TriggerCommands,
    settings: &Settings,
) -> Result<()> {
    info!("Listening for keyboard events");
    let mut parser = TagParser::default();
    let mut is_shifted = false;
    let mut is_ctrl = false;

    let (cmd_tx, cmd_rx) = mpsc::channel::<CommandMessage>();
    let (kb_tx, kb_rx) = mpsc::channel::<KeyboardMessage>();

    let command_timeout = settings.command_timeout;

    thread::spawn(move || {
        loop {
            if TERMINATE.load(Ordering::Relaxed) {
                break;
            }
            match keyboard_device.read_event() {
                Ok(Some(event)) => {
                    if kb_tx.send(KeyboardMessage::Event(event)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    // Interrupted by signal, check terminate flag and retry.
                    continue;
                }
                Err(e) => {
                    let _ = kb_tx.send(KeyboardMessage::Error(e.to_string()));
                    break;
                }
            }
        }
    });

    while !TERMINATE.load(Ordering::Relaxed) {
        drain_command_results(&cmd_rx, &mut clipboard, &mut virtual_device, settings);

        let event = match next_keyboard_event(&kb_rx) {
            NextEvent::Some(e) => e,
            NextEvent::None => continue,
            NextEvent::Stop => break,
        };

        if event.r#type != EV_KEY {
            continue;
        }

        let code = event.code as u32;
        let value = event.value; // 0 = release, 1 = press, 2 = repeat

        // ---- Track modifier state -----------------------------------------
        if code == KEY_LEFTCTRL || code == KEY_RIGHTCTRL {
            is_ctrl = value != 0;
        }
        if code == KEY_LEFTSHIFT || code == KEY_RIGHTSHIFT {
            is_shifted = value != 0;
        }

        // Ignore modifier presses for further processing.
        match code {
            KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTSHIFT | KEY_RIGHTSHIFT => continue,
            _ => {}
        }

        // ---- Intercept Ctrl+C -----------------------------------------
        if is_ctrl && code == KEY_C && value == 1 {
            thread::sleep(Duration::from_millis(settings.clipboard_read_delay_ms));

            let captured = match clipboard.get_text() {
                Ok(t) => t,
                Err(_) => {
                    warn!("Clipboard does not contain valid text");
                    continue;
                }
            };

            spawn_clipboard_command(cmd_tx.clone(), trigger_commands, &captured, command_timeout);
            continue;
        }

        // ---- Normal tag parsing path -----------------------------------

        // Only key presses (value 1) drive tag parsing.
        if value == 0 || value == 2 {
            continue;
        }

        if !is_supported_key_code(code) {
            if code == KEY_BACKSPACE {
                parser.remove_char();
            } else {
                parser = TagParser::default();
            }
            continue;
        }

        let Some(c) = get_char_from_keycode(code, is_shifted) else {
            parser = TagParser::default();
            continue;
        };

        feed_parser_char(
            &mut parser,
            c,
            &cmd_tx,
            trigger_commands,
            command_timeout,
        );
    }

    info!("Shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::clipboard::ClipboardOps;
    use crate::device::KeyInjector;

    use super::*;
    use std::cell::RefCell;

    // ---- parse_tag ----------------------------------------------------

    #[test]
    fn parse_tag_basic() {
        assert_eq!(
            parse_tag("greet:?world", ":?"),
            Some(("greet", Some("world")))
        );
    }

    #[test]
    fn parse_tag_no_separator() {
        assert_eq!(parse_tag("greet world", ":?"), None);
    }

    #[test]
    fn parse_tag_empty_tag_rejected() {
        assert_eq!(parse_tag(":?world", ":?"), None);
    }

    #[test]
    fn parse_tag_empty_input() {
        assert_eq!(parse_tag("", ":?"), None);
    }

    #[test]
    fn parse_tag_empty_arg_is_none() {
        assert_eq!(parse_tag("greet:?", ":?"), Some(("greet", None)));
    }

    #[test]
    fn parse_tag_only_first_separator_used() {
        assert_eq!(
            parse_tag("greet:?a:?b", ":?"),
            Some(("greet", Some("a:?b")))
        );
    }

    #[test]
    fn parse_tag_separator_within_arg_only() {
        // Separator appears only after a valid tag boundary once.
        assert_eq!(
            parse_tag("cmd:?run:?now", ":?"),
            Some(("cmd", Some("run:?now")))
        );
    }

    // ---- resolve_clipboard_command -------------------------------------

    fn triggers(pairs: &[(&str, &[&str])]) -> TriggerCommands {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn resolve_clipboard_command_no_tag() {
        let commands = triggers(&[]);
        let result = resolve_clipboard_command(&commands, "no separator here");
        assert_eq!(result, Err(ClipboardCommandError::NoTag));
    }

    #[test]
    fn resolve_clipboard_command_unknown_tag() {
        let commands = triggers(&[("known", &["echo", "{}"])]);
        let result = resolve_clipboard_command(&commands, "unknown:?arg");
        assert!(matches!(
            result,
            Err(ClipboardCommandError::UnknownTag { .. })
        ));
    }

    #[test]
    fn resolve_clipboard_command_empty_command_treated_as_unknown() {
        let commands = triggers(&[("empty", &[])]);
        let result = resolve_clipboard_command(&commands, "empty:?arg");
        assert!(matches!(
            result,
            Err(ClipboardCommandError::UnknownTag { .. })
        ));
    }

    #[test]
    fn resolve_clipboard_command_trims_tag_name() {
        let commands = triggers(&[("greet", &["echo", "{}"])]);
        // parse_tag doesn't trim, but lookup does via tag_name.trim().
        let result = resolve_clipboard_command(&commands, " greet :?world");
        // tag_name here is " greet " (up to separator), trimmed to "greet"
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_clipboard_command_expands_successfully() {
        let commands = triggers(&[("greet", &["echo", "{}"])]);
        let result = resolve_clipboard_command(&commands, "greet:?world").unwrap();
        assert_eq!(result, vec!["echo", "world"]);
    }

    #[test]
    fn resolve_clipboard_command_no_arg_skips_expansion() {
        let commands = triggers(&[("date", &["date", "+%Y%m%d"])]);
        let result = resolve_clipboard_command(&commands, "date:?").unwrap();
        assert_eq!(result, vec!["date", "+%Y%m%d"]);
    }

    #[test]
    fn resolve_clipboard_command_no_arg_with_placeholder_errors() {
        let commands = triggers(&[("greet", &["echo", "{}"])]);
        let result: std::prelude::v1::Result<Vec<String>, ClipboardCommandError> =
            resolve_clipboard_command(&commands, "greet:?");
        assert!(matches!(
            result,
            Err(ClipboardCommandError::ExpansionFailed { .. })
        ));
    }

    #[test]
    fn resolve_clipboard_command_expansion_failure_on_mismatch() {
        // Two placeholders, only one replacement available -> should error,
        // not silently drop or duplicate (per command.rs's expand_placeholders).
        let commands = triggers(&[("dup", &["echo", "{}", "{}"])]);
        let result: std::prelude::v1::Result<Vec<String>, ClipboardCommandError> =
            resolve_clipboard_command(&commands, "dup:?value");
        assert!(matches!(
            result,
            Err(ClipboardCommandError::ExpansionFailed { .. })
        ));
    }

    // ---- resolve_tag_expansion ------------------------------------------

    #[test]
    fn resolve_tag_expansion_no_content_returns_command_unchanged() {
        let command = vec!["date".to_string(), "+%s".to_string()];
        let result = resolve_tag_expansion(&command[0], &command[1..], None).unwrap();
        assert_eq!(result, command);
    }

    #[test]
    fn resolve_tag_expansion_with_content_substitutes() {
        let command = ["echo".to_string(), "{}".to_string()];
        let result = resolve_tag_expansion(&command[0], &command[1..], Some("hello")).unwrap();
        assert_eq!(result, vec!["echo", "hello"]);
    }

    #[test]
    fn resolve_tag_expansion_mismatch_errors() {
        let command = ["echo".to_string(), "{} {}".to_string()];
        let result = resolve_tag_expansion(&command[0], &command[1..], Some("only-one"));
        assert!(result.is_err());
    }

    // ---- Fakes for ClipboardOps / KeyInjector ----------------------------

    #[derive(Default)]
    struct FakeClipboard {
        text: Option<String>,
        get_fails: bool,
        set_fails: bool,
        history: Vec<String>,
    }

    impl FakeClipboard {
        fn with_text(text: &str) -> Self {
            Self {
                text: Some(text.to_string()),
                ..Default::default()
            }
        }
    }

    impl ClipboardOps for FakeClipboard {
        fn get_text(&mut self) -> std::result::Result<String, String> {
            if self.get_fails {
                return Err("simulated get failure".to_string());
            }
            self.text.clone().ok_or_else(|| "empty".to_string())
        }
        fn set_text(&mut self, text: &str) -> std::result::Result<(), String> {
            if self.set_fails {
                return Err("simulated set failure".to_string());
            }
            self.text = Some(text.to_string());
            self.history.push(text.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeKeyInjector {
        calls: Vec<&'static str>,
        fail_on: Option<&'static str>,
    }

    impl FakeKeyInjector {
        fn failing_at(call: &'static str) -> Self {
            Self {
                fail_on: Some(call),
                ..Default::default()
            }
        }

        fn maybe_fail(&mut self, name: &'static str) -> Result<()> {
            self.calls.push(name);
            if self.fail_on == Some(name) {
                return Err(crate::error::BaanError::Write {
                    detail: "simulated failure".to_string(),
                    source: std::io::Error::other("boom"),
                });
            }
            Ok(())
        }
    }

    impl KeyInjector for FakeKeyInjector {
        fn select_line(&mut self) -> Result<()> {
            self.maybe_fail("select_line")
        }
        fn send_ctrl_c(&mut self) -> Result<()> {
            self.maybe_fail("send_ctrl_c")
        }
        fn send_ctrl_v(&mut self) -> Result<()> {
            self.maybe_fail("send_ctrl_v")
        }
        fn send_string(&mut self, _s: &str) -> Result<()> {
            self.maybe_fail("send_string")
        }
        fn position_at_tag(&mut self, _pos: usize, _len: usize) -> Result<()> {
            self.maybe_fail("position_at_tag")
        }
    }

    fn test_settings() -> Settings {
        Settings {
            flush_delay_ms: 0,
            clipboard_read_delay_ms: 0,
            clipboard_write_delay_ms: 0,
            command_timeout: Duration::from_secs(15),
        }
    }

    // ---- handle_set_clipboard --------------------------------------------

    #[test]
    fn handle_set_clipboard_trims_and_pastes() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        handle_set_clipboard(
            "result\n\n".to_string(),
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert_eq!(clipboard.history.as_slice(), ["result"]);
        assert_eq!(injector.calls.as_slice(), ["send_ctrl_v"]);
    }

    #[test]
    fn handle_set_clipboard_does_not_paste_if_set_fails() {
        let mut clipboard = FakeClipboard {
            set_fails: true,
            ..Default::default()
        };
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        handle_set_clipboard(
            "result".to_string(),
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert!(
            injector.calls.is_empty(),
            "must not paste after a failed clipboard write"
        );
    }

    // ---- inject_replacement ----------------------------------------------

    #[test]
    fn inject_replacement_ascii_types_directly() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        let used_clipboard =
            inject_replacement("hello world", &mut clipboard, &mut injector, &settings).unwrap();

        assert!(!used_clipboard);
        assert_eq!(injector.calls.as_slice(), ["send_string"]);
        assert!(
            clipboard.history.is_empty(),
            "ASCII path must not touch the clipboard"
        );
    }

    #[test]
    fn inject_replacement_non_ascii_uses_clipboard() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        let used_clipboard =
            inject_replacement("héllo", &mut clipboard, &mut injector, &settings).unwrap();

        assert!(used_clipboard);
        assert_eq!(clipboard.history.as_slice(), ["héllo"]);
        assert_eq!(injector.calls.as_slice(), ["send_ctrl_v"]);
    }

    #[test]
    fn inject_replacement_propagates_send_string_failure() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::failing_at("send_string");
        let settings = test_settings();

        let result = inject_replacement("plain text", &mut clipboard, &mut injector, &settings);
        assert!(result.is_err());
    }

    // ---- handle_replace_tag ------------------------------------------------

    #[test]
    fn handle_replace_tag_full_happy_path_restores_old_clipboard() {
        let mut clipboard = FakeClipboard::with_text("old clipboard content");
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        // After Ctrl+C the fake clipboard would contain the copied line;
        // simulate that by pre-seeding what get_text() returns at that
        // point via a clipboard whose content already looks like a line
        // containing the tag.
        clipboard.text = Some("prefix {{tag}} suffix".to_string());

        handle_replace_tag(
            "{{tag}}".to_string(),
            "REPLACED".to_string(),
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert_eq!(
            injector.calls.as_slice(),
            [
                "select_line",
                "send_ctrl_c",
                "position_at_tag",
                "send_string"
            ]
        );
    }

    #[test]
    fn handle_replace_tag_aborts_if_tag_not_found_in_line() {
        let mut clipboard = FakeClipboard::with_text("this line has no tag");
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        handle_replace_tag(
            "{{missing}}".to_string(),
            "REPLACED".to_string(),
            &mut clipboard,
            &mut injector,
            &settings,
        );

        // Should have selected/copied the line to look for the tag, but
        // never attempted to position the cursor or type anything.
        assert_eq!(injector.calls.as_slice(), ["select_line", "send_ctrl_c"]);
    }

    #[test]
    fn handle_replace_tag_aborts_cleanly_if_select_line_fails() {
        let mut clipboard = FakeClipboard::with_text("prefix {{tag}} suffix");
        let mut injector = FakeKeyInjector::failing_at("select_line");
        let settings = test_settings();

        handle_replace_tag(
            "{{tag}}".to_string(),
            "REPLACED".to_string(),
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert_eq!(injector.calls.as_slice(), ["select_line"]);
    }

    #[test]
    fn handle_replace_tag_computes_char_offsets_not_byte_offsets() {
        // "héllo " has a 2-byte 'é', so byte offset != char offset for
        // anything after it. position_at_tag must receive char counts.
        let mut clipboard = FakeClipboard::with_text("héllo {{tag}}");
        let injector = RefCell::new(FakeKeyInjector::default());

        struct RecordingInjector<'a>(&'a RefCell<FakeKeyInjector>);
        impl<'a> KeyInjector for RecordingInjector<'a> {
            fn select_line(&mut self) -> Result<()> {
                self.0.borrow_mut().select_line()
            }
            fn send_ctrl_c(&mut self) -> Result<()> {
                self.0.borrow_mut().send_ctrl_c()
            }
            fn send_ctrl_v(&mut self) -> Result<()> {
                self.0.borrow_mut().send_ctrl_v()
            }
            fn send_string(&mut self, s: &str) -> Result<()> {
                self.0.borrow_mut().send_string(s)
            }
            fn position_at_tag(&mut self, pos: usize, len: usize) -> Result<()> {
                // "héllo " is 6 chars before the tag.
                assert_eq!(pos, 6);
                assert_eq!(len, "{{tag}}".chars().count());
                self.0.borrow_mut().position_at_tag(pos, len)
            }
        }

        let settings = test_settings();
        let mut rec = RecordingInjector(&injector);
        handle_replace_tag(
            "{{tag}}".to_string(),
            "x".to_string(),
            &mut clipboard,
            &mut rec,
            &settings,
        );
    }
}
