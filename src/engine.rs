//! The keyboard event loop and command injection.
//!
//! A background thread ([`spawn_keyboard_reader`]) reads raw key events from
//! the physical keyboard and forwards them to the main loop
//! ([`run_keyboard_event_loop`]). The loop tracks modifier state, intercepts
//! copy gestures (Ctrl+C / Ctrl+Shift+C) to trigger clipboard commands, and
//! feeds typed characters into the [`TagParser`]. When a tag matches a
//! configured trigger, the command runs on a background thread and its output
//! comes back as a [`CommandResult`], which the loop applies by typing or
//! pasting into the focused application.

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

/// Which kind of application is the injection target.
///
/// Terminals differ from GUI apps in two ways that matter here: they use
/// Ctrl+Shift+C / Ctrl+Shift+V instead of Ctrl+C / Ctrl+V, and they don't
/// support Home/End-based keyboard selection. The target is inferred from the
/// user's own input (all-caps tag names, the Ctrl+Shift+C gesture) rather
/// than from window detection, which is not reliably available on every
/// compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// A regular desktop application.
    Gui,
    /// A terminal emulator.
    Terminal,
}

/// Messages sent from the keyboard reader thread to the main event loop.
enum KeyboardReaderMessage {
    /// A keyboard event was read successfully.
    Event(InputEvent),
    /// The keyboard read returned an error.
    Error(String),
}

/// Command results sent back from background threads to the event loop.
enum CommandResult {
    /// A tag was expanded inside the current line; replace the tag text.
    ReplaceTag {
        tag_text: String,
        output: String,
        target: TargetKind,
    },
    /// A clipboard command finished; put the output on the clipboard and paste.
    /// `trigger_len` is the length (in characters) of the `name:?arg` trigger
    /// text that was captured — needed so terminal targets can backspace it
    /// off the command line before pasting.
    SetClipboard {
        output: String,
        target: TargetKind,
        trigger_len: usize,
    },
}

/// Reason a clipboard-triggered command couldn't be resolved into a
/// runnable argv, extracted so tests can check *why* without spawning
/// a thread or running a process.
#[derive(Debug, PartialEq, Eq)]
enum ClipboardTagError {
    NoTag,
    UnknownTag { tag_name: String },
    ExpansionFailed { tag_name: String, detail: String },
}

/// Runs the main keyboard event loop until `TERMINATE` is set.
///
/// Opens no devices itself — the caller provides the physical keyboard
/// reader, the virtual injection device, and the clipboard.
pub fn run_keyboard_event_loop(
    mut clipboard: Clipboard,
    keyboard_device: KeyboardDevice,
    mut virtual_device: VirtualDevice,
    trigger_commands: &TriggerCommands,
    settings: &Settings,
) -> Result<()> {
    info!("Listening for keyboard events");
    let mut parser = TagParser::default();
    let mut is_shifted = false;
    let mut is_ctrl = false;

    // Commands run on background threads and send their results back here.
    let (result_tx, result_rx) = mpsc::channel::<CommandResult>();
    // The keyboard reader thread forwards raw events here.
    let (reader_tx, reader_rx) = mpsc::channel::<KeyboardReaderMessage>();

    let command_timeout = settings.command_timeout;

    spawn_keyboard_reader(keyboard_device, reader_tx);

    while !TERMINATE.load(Ordering::Relaxed) {
        drain_command_results(&result_rx, &mut clipboard, &mut virtual_device, settings);

        let event = match next_keyboard_event(&reader_rx) {
            NextKeyboardEvent::Some(e) => e,
            NextKeyboardEvent::None => continue,
            NextKeyboardEvent::Stop => break,
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

        // ---- Intercept copy gestures --------------------------------------
        //
        // Ctrl+C is the copy gesture in GUI apps; Ctrl+Shift+C is the copy
        // gesture in terminals (where plain Ctrl+C means SIGINT). Both are
        // checked for a `name:?arg` tag in the clipboard; the inferred target
        // decides which paste shortcut the result is injected with.
        if is_ctrl && code == KEY_C && value == 1 {
            intercept_clipboard_trigger(
                is_shifted,
                &mut clipboard,
                &result_tx,
                trigger_commands,
                command_timeout,
                Duration::from_millis(settings.clipboard_read_delay_ms),
            );
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
            &result_tx,
            trigger_commands,
            command_timeout,
        );
    }

    info!("Shutting down");
    Ok(())
}

/// Handles a copy gesture (Ctrl+C or Ctrl+Shift+C): waits for the copy to
/// land, reads the clipboard, and if it holds a `name:?arg` tag, spawns the
/// trigger's command on a background thread. The event is always consumed,
/// even when the clipboard holds no tag.
fn intercept_clipboard_trigger<C: ClipboardOps>(
    is_shifted: bool,
    clipboard: &mut C,
    result_tx: &mpsc::Sender<CommandResult>,
    trigger_commands: &TriggerCommands,
    command_timeout: Duration,
    clipboard_read_delay: Duration,
) {
    thread::sleep(clipboard_read_delay);

    let captured_text = match clipboard.get_text() {
        Ok(t) => t,
        Err(_) => {
            warn!("Clipboard does not contain valid text");
            return;
        }
    };

    if captured_text.trim().is_empty() {
        debug!("Clipboard is empty, nothing to run on copy");
        return;
    }

    let target = infer_clipboard_target(is_shifted, &captured_text);
    spawn_clipboard_command(
        result_tx.clone(),
        trigger_commands,
        &captured_text,
        command_timeout,
        target,
    );
}

/// Resolves `captured_text` into a runnable argv: parse the tag, look up its
/// trigger, and expand placeholders. Pure — no threads or process execution.
fn resolve_clipboard_command(
    trigger_commands: &TriggerCommands,
    captured_text: &str,
) -> std::result::Result<Vec<String>, ClipboardTagError> {
    let (tag_name, arg) = parse_tag(captured_text, ":?").ok_or(ClipboardTagError::NoTag)?;
    let command = match lookup_trigger(trigger_commands, tag_name.trim()) {
        Some(cmd) if !cmd.is_empty() => cmd,
        _ => {
            return Err(ClipboardTagError::UnknownTag {
                tag_name: tag_name.to_string(),
            });
        }
    };

    match arg {
        Some(a) => expand_placeholders(&command[0], &command[1..], &[a]).map_err(|e| {
            ClipboardTagError::ExpansionFailed {
                tag_name: tag_name.to_string(),
                detail: e.to_string(),
            }
        }),
        None if has_placeholders(command) => Err(ClipboardTagError::ExpansionFailed {
            tag_name: tag_name.to_string(),
            detail: "Command requires an argument but none was provided".to_string(),
        }),
        None => Ok(command.to_vec()),
    }
}

/// Case-insensitive lookup of a trigger command.
///
/// Config parsing rejects triggers that differ only by letter case, so at
/// most one key can match `name`.
fn lookup_trigger<'a>(
    trigger_commands: &'a TriggerCommands,
    name: &str,
) -> Option<&'a Vec<String>> {
    trigger_commands
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, command)| command)
}

/// Target inferred from a typed tag's name: a name with no lowercase letters
/// (e.g. `<HI/>`, `<BASE64/>`) is the user's signal that they are typing in a
/// terminal.
fn infer_target_from_tag_name(tag_name: &str) -> TargetKind {
    if tag_name.chars().all(|c| !c.is_ascii_lowercase()) {
        TargetKind::Terminal
    } else {
        TargetKind::Gui
    }
}

/// Target inferred for a clipboard-triggered command: Ctrl+Shift+C is the
/// terminal copy gesture, so a shifted Ctrl+C always means terminal. Without
/// shift, the all-caps tag-name convention applies (e.g. `NAME:?arg`).
fn infer_clipboard_target(is_shifted: bool, captured: &str) -> TargetKind {
    if is_shifted {
        return TargetKind::Terminal;
    }
    match parse_tag(captured, ":?").map(|(name, _)| name.trim()) {
        Some(name) if infer_target_from_tag_name(name) == TargetKind::Terminal => {
            TargetKind::Terminal
        }
        _ => TargetKind::Gui,
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
/// back as `CommandResult::SetClipboard` so the event loop stays responsive.
fn spawn_clipboard_command(
    result_tx: mpsc::Sender<CommandResult>,
    trigger_commands: &TriggerCommands,
    captured_text: &str,
    timeout: Duration,
    target: TargetKind,
) {
    let expanded = match resolve_clipboard_command(trigger_commands, captured_text) {
        Ok(expanded) => expanded,
        Err(ClipboardTagError::NoTag) => {
            // Expected case: most copied text isn't a `name:?arg` tag (and
            // Ctrl+Shift+C is used for any terminal copy), so don't spam logs.
            debug!("Clipboard text does not contain a valid tag");
            return;
        }
        Err(ClipboardTagError::UnknownTag { tag_name }) => {
            warn!(tag = %tag_name, "No command configured for tag");
            return;
        }
        Err(ClipboardTagError::ExpansionFailed { tag_name, detail }) => {
            warn!(tag = %tag_name, detail = %detail, "Failed to expand command placeholders");
            return;
        }
    };

    let trigger_len = captured_text.chars().count();

    let tx = result_tx.clone();
    thread::spawn(
        move || match run_command(&expanded[0], &expanded[1..], timeout) {
            Ok(output) => {
                let _ = tx.send(CommandResult::SetClipboard {
                    output,
                    target,
                    trigger_len,
                });
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
    result_tx: mpsc::Sender<CommandResult>,
    tag_name: String,
    content: Option<String>,
    tag_text: String,
    command: String,
    options: Vec<String>,
    timeout: Duration,
    target: TargetKind,
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
                let _ = result_tx.send(CommandResult::ReplaceTag {
                    tag_text,
                    output,
                    target,
                });
            }
            Err(err) => {
                error!(tag = %tag_name, detail = %err, "Failed to execute command");
            }
        }
    });
}

/// Simulates the paste shortcut for `target`: Ctrl+V in GUI apps, Ctrl+Shift+V
/// in terminals.
fn paste_shortcut<V: KeyInjector>(virtual_device: &mut V, target: TargetKind) -> Result<()> {
    match target {
        TargetKind::Gui => virtual_device.send_ctrl_v(),
        TargetKind::Terminal => virtual_device.send_ctrl_shift_v(),
    }
}

/// Applies a `SetClipboard` result: in terminals, first deletes the trigger
/// text (the `name:?arg` the user had on the command line — paste doesn't
/// replace a selection there like it does in GUI apps), then puts `output` on
/// the clipboard and pastes with the shortcut for `target`.
fn handle_set_clipboard<C, V>(
    output: String,
    target: TargetKind,
    trigger_len: usize,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) where
    C: ClipboardOps,
    V: KeyInjector,
{
    // In a terminal the trigger text stays visible after the copy gesture, so
    // backspace it away before pasting the output. A failure here is logged
    // but doesn't stop the paste — the output is on the clipboard either way.
    if target == TargetKind::Terminal {
        if let Err(e) = virtual_device.send_backspace(trigger_len) {
            error!(detail = %e, "Failed to delete trigger text in terminal");
        }
    }

    let trimmed = output.trim_end().to_owned();
    if let Err(e) = clipboard.set_text(&trimmed) {
        error!(detail = %e, "Failed to set clipboard text");
        return;
    }

    // Wait for clipboard ownership to register before pasting.
    thread::sleep(Duration::from_millis(settings.clipboard_write_delay_ms));

    if let Err(e) = paste_shortcut(virtual_device, target) {
        error!(detail = %e, "Failed to simulate paste");
    }
}

/// Types `replacement` directly if it's ASCII, otherwise round-trips it
/// through the clipboard (for characters the virtual keyboard can't emit
/// directly) and pastes with the shortcut for `target`.
///
/// Returns whether the clipboard was used, so the caller knows whether to
/// wait before restoring the previous clipboard contents.
fn inject_replacement<C, V>(
    replacement: &str,
    target: TargetKind,
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
    paste_shortcut(virtual_device, target)?;
    Ok(true)
}

/// Replaces `tag_text` on the current line with `output`.
///
/// GUI path: selects and copies the line to locate the tag, moves the cursor
/// to it, deletes it, and types/pastes the replacement — then restores
/// whatever was on the clipboard beforehand.
///
/// Terminal path: terminals can't select/copy a line via keyboard (no
/// Home/End support, Ctrl+C is SIGINT), so instead we rely on the fact that
/// the cursor is directly after the tag when it fired: delete it with plain
/// backspaces and type/paste the replacement.
fn handle_replace_tag<C, V>(
    tag_text: String,
    output: String,
    target: TargetKind,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) where
    C: ClipboardOps,
    V: KeyInjector,
{
    debug!(tag = %tag_text, "Command output received from background thread");
    thread::sleep(Duration::from_millis(settings.flush_delay_ms));

    let replacement = output.trim_end();

    if target == TargetKind::Terminal {
        // Save the clipboard so it can be restored; default to empty on failure.
        let old_clipboard = clipboard
            .get_text()
            .map(|s| s.to_string())
            .unwrap_or_default();

        if let Err(e) = virtual_device.send_backspace(tag_text.chars().count()) {
            error!(detail = %e, "Failed to delete tag in terminal");
            return;
        }

        let pasted_from_clipboard =
            match inject_replacement(replacement, target, clipboard, virtual_device, settings) {
                Ok(used_clipboard) => used_clipboard,
                Err(e) => {
                    error!(detail = %e, "Failed to inject replacement text");
                    return;
                }
            };

        // Only when the replacement was pasted from the clipboard (non-ASCII)
        // do we need to restore what was there before — the ASCII path never
        // touched the clipboard, so leave it alone.
        if pasted_from_clipboard {
            // Wait so the app's paste request is served first.
            thread::sleep(Duration::from_millis(settings.clipboard_write_delay_ms));
            if let Err(e) = clipboard.set_text(&old_clipboard) {
                error!(detail = %e, "Failed to set clipboard old value");
            }
        }
        return;
    }

    // ---- GUI path ------------------------------------------------------
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

    if let Err(e) = virtual_device.position_at_tag(pos_chars, tag_len_chars) {
        error!(detail = %e, "Failed to position cursor at tag");
        return;
    }

    let pasted_from_clipboard =
        match inject_replacement(replacement, target, clipboard, virtual_device, settings) {
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
    result_rx: &mpsc::Receiver<CommandResult>,
    clipboard: &mut C,
    virtual_device: &mut V,
    settings: &Settings,
) where
    C: ClipboardOps,
    V: KeyInjector,
{
    while let Ok(result) = result_rx.try_recv() {
        match result {
            CommandResult::SetClipboard {
                output,
                target,
                trigger_len,
            } => {
                handle_set_clipboard(
                    output,
                    target,
                    trigger_len,
                    clipboard,
                    virtual_device,
                    settings,
                );
            }
            CommandResult::ReplaceTag {
                tag_text,
                output,
                target,
            } => {
                handle_replace_tag(
                    tag_text,
                    output,
                    target,
                    clipboard,
                    virtual_device,
                    settings,
                );
            }
        }
    }
}

/// Outcome of trying to fetch the next keyboard event.
enum NextKeyboardEvent {
    /// A key event to process.
    Some(InputEvent),
    /// No event yet; caller should loop back around.
    None,
    /// The reader thread ended (error or disconnect); caller should stop.
    Stop,
}

/// Spawns the background thread that reads keyboard events and forwards
/// them to the main loop via `reader_tx`. Stops when the reader errors, the
/// channel disconnects, or `TERMINATE` is set.
fn spawn_keyboard_reader(
    mut keyboard_device: KeyboardDevice,
    reader_tx: mpsc::Sender<KeyboardReaderMessage>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        loop {
            if TERMINATE.load(Ordering::Relaxed) {
                break;
            }
            match keyboard_device.read_event() {
                Ok(Some(event)) => {
                    if reader_tx.send(KeyboardReaderMessage::Event(event)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    // Interrupted by signal, check terminate flag and retry.
                    continue;
                }
                Err(e) => {
                    let _ = reader_tx.send(KeyboardReaderMessage::Error(e.to_string()));
                    break;
                }
            }
        }
    })
}

fn next_keyboard_event(reader_rx: &mpsc::Receiver<KeyboardReaderMessage>) -> NextKeyboardEvent {
    match reader_rx.recv_timeout(Duration::from_millis(10)) {
        Ok(KeyboardReaderMessage::Event(e)) => NextKeyboardEvent::Some(e),
        Ok(KeyboardReaderMessage::Error(e)) => {
            error!(detail = %e, "Keyboard read error");
            NextKeyboardEvent::Stop
        }
        Err(mpsc::RecvTimeoutError::Timeout) => NextKeyboardEvent::None,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            error!("Keyboard reader thread disconnected");
            NextKeyboardEvent::Stop
        }
    }
}

/// Feeds `c` into the tag parser and, if a tag now matches a configured
/// trigger, spawns its command in the background.
fn feed_parser_char(
    parser: &mut TagParser,
    c: char,
    result_tx: &mpsc::Sender<CommandResult>,
    trigger_commands: &TriggerCommands,
    timeout: Duration,
) {
    parser.consume(c);

    if let Some((tag_name, content, tag_text)) = parser.take() {
        let tag_name_trimmed = tag_name.trim();
        let command = match lookup_trigger(trigger_commands, tag_name_trimmed) {
            Some(c) if !c.is_empty() => c,
            _ => return,
        };

        // The tag's casing tells us the target: all-caps names (e.g. `<HI/>`)
        // are the user's signal that they are in a terminal.
        let target = infer_target_from_tag_name(tag_name_trimmed);

        let cmd = command[0].clone();
        let options = command[1..].to_vec();

        spawn_tag_command(
            result_tx.clone(),
            tag_name_trimmed.to_string(),
            content,
            tag_text,
            cmd,
            options,
            timeout,
            target,
        );
    }
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
        assert_eq!(result, Err(ClipboardTagError::NoTag));
    }

    #[test]
    fn resolve_clipboard_command_unknown_tag() {
        let commands = triggers(&[("known", &["echo", "{}"])]);
        let result = resolve_clipboard_command(&commands, "unknown:?arg");
        assert!(matches!(result, Err(ClipboardTagError::UnknownTag { .. })));
    }

    #[test]
    fn resolve_clipboard_command_empty_command_treated_as_unknown() {
        let commands = triggers(&[("empty", &[])]);
        let result = resolve_clipboard_command(&commands, "empty:?arg");
        assert!(matches!(result, Err(ClipboardTagError::UnknownTag { .. })));
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
        let result: std::prelude::v1::Result<Vec<String>, ClipboardTagError> =
            resolve_clipboard_command(&commands, "greet:?");
        assert!(matches!(
            result,
            Err(ClipboardTagError::ExpansionFailed { .. })
        ));
    }

    #[test]
    fn resolve_clipboard_command_expansion_failure_on_mismatch() {
        // Two placeholders, only one replacement available -> should error,
        // not silently drop or duplicate (per command.rs's expand_placeholders).
        let commands = triggers(&[("dup", &["echo", "{}", "{}"])]);
        let result: std::prelude::v1::Result<Vec<String>, ClipboardTagError> =
            resolve_clipboard_command(&commands, "dup:?value");
        assert!(matches!(
            result,
            Err(ClipboardTagError::ExpansionFailed { .. })
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
        calls: Vec<String>,
        fail_on: Option<&'static str>,
    }

    impl FakeKeyInjector {
        fn failing_at(call: &'static str) -> Self {
            Self {
                fail_on: Some(call),
                ..Default::default()
            }
        }

        fn maybe_fail(&mut self, name: &str) -> Result<()> {
            self.calls.push(name.to_string());
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
        fn send_ctrl_shift_c(&mut self) -> Result<()> {
            self.maybe_fail("send_ctrl_shift_c")
        }
        fn send_ctrl_v(&mut self) -> Result<()> {
            self.maybe_fail("send_ctrl_v")
        }
        fn send_ctrl_shift_v(&mut self) -> Result<()> {
            self.maybe_fail("send_ctrl_shift_v")
        }
        fn send_string(&mut self, _s: &str) -> Result<()> {
            self.maybe_fail("send_string")
        }
        fn position_at_tag(&mut self, _pos: usize, _len: usize) -> Result<()> {
            self.maybe_fail("position_at_tag")
        }
        fn send_backspace(&mut self, n: usize) -> Result<()> {
            self.calls.push(format!("send_backspace({n})"));
            if self.fail_on == Some("send_backspace") {
                return Err(crate::error::BaanError::Write {
                    detail: "simulated failure".to_string(),
                    source: std::io::Error::other("boom"),
                });
            }
            Ok(())
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
            TargetKind::Gui,
            9,
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert_eq!(clipboard.history.as_slice(), ["result"]);
        // GUI target ignores trigger_len — no backspace is sent.
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
            TargetKind::Gui,
            9,
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

        let used_clipboard = inject_replacement(
            "hello world",
            TargetKind::Gui,
            &mut clipboard,
            &mut injector,
            &settings,
        )
        .unwrap();

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

        let used_clipboard = inject_replacement(
            "héllo",
            TargetKind::Gui,
            &mut clipboard,
            &mut injector,
            &settings,
        )
        .unwrap();

        assert!(used_clipboard);
        assert_eq!(clipboard.history.as_slice(), ["héllo"]);
        assert_eq!(injector.calls.as_slice(), ["send_ctrl_v"]);
    }

    #[test]
    fn inject_replacement_propagates_send_string_failure() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::failing_at("send_string");
        let settings = test_settings();

        let result = inject_replacement(
            "plain text",
            TargetKind::Gui,
            &mut clipboard,
            &mut injector,
            &settings,
        );
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
            TargetKind::Gui,
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
            TargetKind::Gui,
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
            TargetKind::Gui,
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
            fn send_ctrl_shift_c(&mut self) -> Result<()> {
                self.0.borrow_mut().send_ctrl_shift_c()
            }
            fn send_ctrl_v(&mut self) -> Result<()> {
                self.0.borrow_mut().send_ctrl_v()
            }
            fn send_ctrl_shift_v(&mut self) -> Result<()> {
                self.0.borrow_mut().send_ctrl_shift_v()
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
            fn send_backspace(&mut self, n: usize) -> Result<()> {
                self.0.borrow_mut().send_backspace(n)
            }
        }

        let settings = test_settings();
        let mut rec = RecordingInjector(&injector);
        handle_replace_tag(
            "{{tag}}".to_string(),
            "x".to_string(),
            TargetKind::Gui,
            &mut clipboard,
            &mut rec,
            &settings,
        );
    }

    // ---- infer_target_from_tag_name --------------------------------------

    #[test]
    fn uppercase_tag_name_targets_terminal() {
        assert_eq!(infer_target_from_tag_name("HI"), TargetKind::Terminal);
        assert_eq!(infer_target_from_tag_name("BASE64"), TargetKind::Terminal);
        assert_eq!(infer_target_from_tag_name("MY-TAG"), TargetKind::Terminal);
        assert_eq!(infer_target_from_tag_name("HI_2"), TargetKind::Terminal);
    }

    #[test]
    fn any_lowercase_letter_targets_gui() {
        assert_eq!(infer_target_from_tag_name("hi"), TargetKind::Gui);
        assert_eq!(infer_target_from_tag_name("base64"), TargetKind::Gui);
        assert_eq!(infer_target_from_tag_name("my-tag"), TargetKind::Gui);
        // Mixed case counts as GUI too — only *no* lowercase means terminal.
        assert_eq!(infer_target_from_tag_name("Hello"), TargetKind::Gui);
        assert_eq!(infer_target_from_tag_name("HIi"), TargetKind::Gui);
    }

    // ---- infer_clipboard_target -------------------------------------------

    #[test]
    fn ctrl_shift_c_always_targets_terminal() {
        // The Ctrl+Shift+C gesture is the terminal copy shortcut regardless of
        // what (or whether) a tag is on the clipboard.
        assert_eq!(
            infer_clipboard_target(true, "name:?arg"),
            TargetKind::Terminal
        );
        assert_eq!(
            infer_clipboard_target(true, "NAME:?arg"),
            TargetKind::Terminal
        );
        assert_eq!(
            infer_clipboard_target(true, "no tag here"),
            TargetKind::Terminal
        );
    }

    #[test]
    fn uppercase_name_targets_terminal_without_shift() {
        // Plain Ctrl+C with an all-caps tag name also implies terminal.
        assert_eq!(
            infer_clipboard_target(false, "NAME:?arg"),
            TargetKind::Terminal
        );
        assert_eq!(
            infer_clipboard_target(false, "BASE64:?x"),
            TargetKind::Terminal
        );
    }

    #[test]
    fn lowercase_name_targets_gui_without_shift() {
        assert_eq!(infer_clipboard_target(false, "name:?arg"), TargetKind::Gui);
        assert_eq!(infer_clipboard_target(false, "nAmE:?arg"), TargetKind::Gui);
        // No tag at all → nothing to target; defaults to GUI.
        assert_eq!(
            infer_clipboard_target(false, "random text"),
            TargetKind::Gui
        );
    }

    // ---- lookup_trigger ---------------------------------------------------

    #[test]
    fn lookup_trigger_matches_case_insensitively() {
        let commands = triggers(&[("greet", &["echo", "{}"])]);
        let expected = vec!["echo".to_string(), "{}".to_string()];
        assert_eq!(lookup_trigger(&commands, "greet").unwrap(), &expected);
        assert_eq!(lookup_trigger(&commands, "GREET").unwrap(), &expected);
        assert_eq!(lookup_trigger(&commands, "GrEeT").unwrap(), &expected);
        assert!(lookup_trigger(&commands, "nope").is_none());
    }

    #[test]
    fn resolve_clipboard_command_matches_uppercase_tag() {
        let commands = triggers(&[("greet", &["echo", "{}"])]);
        let result = resolve_clipboard_command(&commands, "GREET:?world").unwrap();
        assert_eq!(result, vec!["echo", "world"]);
    }

    // ---- handle_set_clipboard: terminal target -----------------------------

    #[test]
    fn handle_set_clipboard_terminal_backspaces_trigger_then_pastes() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        // "name:?arg" is 9 characters of trigger text left on the line.
        handle_set_clipboard(
            "result".to_string(),
            TargetKind::Terminal,
            9,
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert_eq!(clipboard.history.as_slice(), ["result"]);
        // Trigger text deleted first, then pasted with the terminal shortcut.
        assert_eq!(
            injector.calls.as_slice(),
            ["send_backspace(9)", "send_ctrl_shift_v"]
        );
    }

    #[test]
    fn handle_set_clipboard_terminal_backspace_failure_still_pastes() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::failing_at("send_backspace");
        let settings = test_settings();

        handle_set_clipboard(
            "result".to_string(),
            TargetKind::Terminal,
            9,
            &mut clipboard,
            &mut injector,
            &settings,
        );

        // Backspace failure is logged but doesn't block the paste — the
        // output is on the clipboard either way.
        assert_eq!(clipboard.history.as_slice(), ["result"]);
        assert_eq!(
            injector.calls.as_slice(),
            ["send_backspace(9)", "send_ctrl_shift_v"]
        );
    }

    // ---- inject_replacement: terminal target --------------------------------

    #[test]
    fn inject_replacement_ascii_terminal_types_directly() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        let used = inject_replacement(
            "hi",
            TargetKind::Terminal,
            &mut clipboard,
            &mut injector,
            &settings,
        )
        .unwrap();

        assert!(!used);
        assert_eq!(injector.calls.as_slice(), ["send_string"]);
        assert!(clipboard.history.is_empty());
    }

    #[test]
    fn inject_replacement_non_ascii_terminal_pastes_with_ctrl_shift_v() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        let used = inject_replacement(
            "héllo",
            TargetKind::Terminal,
            &mut clipboard,
            &mut injector,
            &settings,
        )
        .unwrap();

        assert!(used);
        assert_eq!(clipboard.history.as_slice(), ["héllo"]);
        assert_eq!(injector.calls.as_slice(), ["send_ctrl_shift_v"]);
    }

    // ---- handle_replace_tag: terminal target --------------------------------

    #[test]
    fn handle_replace_tag_terminal_ascii_backspaces_then_types() {
        let mut clipboard = FakeClipboard::default();
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        handle_replace_tag(
            "<HI/>".to_string(),
            "Hello World!".to_string(),
            TargetKind::Terminal,
            &mut clipboard,
            &mut injector,
            &settings,
        );

        // No Home/End, no select/copy: the 5-char tag is deleted with plain
        // backspaces and the ASCII output is typed directly.
        assert_eq!(
            injector.calls.as_slice(),
            ["send_backspace(5)", "send_string"]
        );
        assert!(
            clipboard.history.is_empty(),
            "ASCII terminal path must not touch the clipboard"
        );
    }

    #[test]
    fn handle_replace_tag_terminal_non_ascii_pastes_and_restores_clipboard() {
        let mut clipboard = FakeClipboard::with_text("old clipboard");
        let mut injector = FakeKeyInjector::default();
        let settings = test_settings();

        handle_replace_tag(
            "<HI/>".to_string(),
            "héllo".to_string(),
            TargetKind::Terminal,
            &mut clipboard,
            &mut injector,
            &settings,
        );

        assert_eq!(
            injector.calls.as_slice(),
            ["send_backspace(5)", "send_ctrl_shift_v"]
        );
        // Replacement written, then the old value restored.
        assert_eq!(clipboard.history.as_slice(), ["héllo", "old clipboard"]);
    }

    #[test]
    fn handle_replace_tag_terminal_aborts_if_backspace_fails() {
        let mut clipboard = FakeClipboard::with_text("prefix <HI/> suffix");
        let mut injector = FakeKeyInjector::failing_at("send_backspace");
        let settings = test_settings();

        handle_replace_tag(
            "<HI/>".to_string(),
            "out".to_string(),
            TargetKind::Terminal,
            &mut clipboard,
            &mut injector,
            &settings,
        );

        // Stops after the failed deletion; never types or touches the clipboard.
        assert_eq!(injector.calls.as_slice(), ["send_backspace(5)"]);
        assert!(clipboard.history.is_empty());
    }
}
