use std::fs::File;
use std::io::{Read, Write as IoWrite};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use tracing::info;

use crate::error::{BaanError, Result};
use crate::event::{InputEvent, UinputSetup};
use crate::keycode::{
    BUS_USB, EV_KEY, KEY_BACKSPACE, KEY_C, KEY_END, KEY_HOME, KEY_LEFTCTRL, KEY_LEFTSHIFT,
    KEY_RIGHT, KEY_V, LINUX_KEYS, SLEEP_TIME_US, UI_DEV_CREATE, UI_DEV_DESTROY, UI_DEV_SETUP,
    UI_SET_EVBIT, UI_SET_KEYBIT, get_key_from_char,
};

const _INPUT_EVENT_SIZE_CHECK: () = assert!(std::mem::size_of::<InputEvent>() == 24);

/// Path to the uinput device.
pub const UINPUT_PATH: &str = "/dev/uinput";

/// Delay between individual key events, giving the receiving application
/// time to process each one before the next arrives.
const STEP_DELAY: Duration = Duration::from_micros(SLEEP_TIME_US);

/// Key-injection operations needed by the event loop. Implemented for
/// `VirtualDevice`; fakeable in tests.
pub trait KeyInjector {
    fn select_line(&mut self) -> Result<()>;
    /// Simulate Ctrl+C (copy).
    fn send_ctrl_c(&mut self) -> Result<()>;
    /// Simulate Ctrl+Shift+C (the copy shortcut in terminals, where plain
    /// Ctrl+C means SIGINT).
    #[allow(dead_code)]
    fn send_ctrl_shift_c(&mut self) -> Result<()>;
    /// Simulate Ctrl+V (paste).
    fn send_ctrl_v(&mut self) -> Result<()>;
    /// Simulate Ctrl+Shift+V (the paste shortcut in terminals).
    fn send_ctrl_shift_v(&mut self) -> Result<()>;
    fn send_string(&mut self, s: &str) -> Result<()>;
    fn position_at_tag(&mut self, pos: usize, len: usize) -> Result<()>;
    /// Press Backspace `n` times, deleting `n` characters before the cursor.
    fn send_backspace(&mut self, n: usize) -> Result<()>;
}

/// A virtual keyboard device backed by uinput.
pub struct VirtualDevice {
    device: File,
}

#[allow(dead_code)]
impl VirtualDevice {
    /// Opens `/dev/uinput`, registers all required keys and creates the
    /// virtual keyboard.
    pub fn new() -> Result<Self> {
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(UINPUT_PATH)
            .map_err(|e| BaanError::Open {
                path: UINPUT_PATH.to_string(),
                source: e,
            })?;

        let fd = device.as_raw_fd();

        unsafe {
            ioctl(
                fd,
                UI_SET_EVBIT,
                EV_KEY as u64,
                |d| BaanError::InitVirtualInput { detail: d },
                "UI_SET_EVBIT(EV_KEY)",
            )?;

            for &code in &LINUX_KEYS {
                ioctl(
                    fd,
                    UI_SET_KEYBIT,
                    code as u64,
                    |d| BaanError::AddKey {
                        keycode: code,
                        detail: d,
                    },
                    &format!("UI_SET_KEYBIT({})", code),
                )?;
            }

            let setup = UinputSetup::new("baan", BUS_USB, 1187, 1999);
            ioctl_ptr(
                fd,
                UI_DEV_SETUP,
                &setup as *const _ as *const libc::c_void,
                |d| BaanError::SetupVirtualDevice { detail: d },
                "UI_DEV_SETUP",
            )?;

            ioctl(
                fd,
                UI_DEV_CREATE,
                0,
                |d| BaanError::CreateVirtualDevice { detail: d },
                "UI_DEV_CREATE",
            )?;
        }

        info!(path = UINPUT_PATH, "Created virtual keyboard device");
        Ok(Self { device })
    }

    /// Write a raw input event to the device.
    fn write_event(&mut self, event: &InputEvent) -> Result<()> {
        self.device
            .write_all(event_bytes(event))
            .map_err(|e| BaanError::Write {
                detail: "virtual device".to_string(),
                source: e,
            })
    }

    /// Send a sync event.
    pub fn send_sync(&mut self) -> Result<()> {
        self.write_event(&InputEvent::sync_event())
    }

    /// Send a key-down event followed by a key-up event for the given keycode.
    pub fn send_key_press(&mut self, keycode: u32) -> Result<()> {
        self.write_event(&InputEvent::key_event(keycode, 1))?;
        self.write_event(&InputEvent::key_event(keycode, 0))?;
        self.send_sync()
    }

    /// Send a key-down event (value=1) for the given keycode.
    pub fn send_key_down(&mut self, keycode: u32) -> Result<()> {
        self.write_event(&InputEvent::key_event(keycode, 1))
    }

    /// Send a key-up event (value=0) for the given keycode.
    pub fn send_key_up(&mut self, keycode: u32) -> Result<()> {
        self.write_event(&InputEvent::key_event(keycode, 0))
    }

    /// Send `n` backspace key presses.
    pub fn send_backspace(&mut self, n: usize) -> Result<()> {
        for _ in 0..n {
            self.write_event(&InputEvent::key_event(KEY_BACKSPACE, 1))?;
            self.write_event(&InputEvent::key_event(KEY_BACKSPACE, 0))?;
        }
        self.send_sync()
    }

    /// Press and release `keycode`, pausing `STEP_DELAY` after each half so
    /// downstream applications reliably observe both edges.
    ///
    /// Internal building block for multi-key sequences below; unlike
    /// `send_key_press`, it does not send a sync — callers batch that.
    fn tap_key(&mut self, keycode: u32) -> Result<()> {
        self.send_key_down(keycode)?;
        thread::sleep(STEP_DELAY);
        self.send_key_up(keycode)?;
        thread::sleep(STEP_DELAY);
        Ok(())
    }

    /// Holds `modifier` down, runs `action`, then releases it.
    ///
    /// The release (and, if `sync_after` is set, a final sync) is *always*
    /// attempted, even if `action` returns an error — otherwise a failed
    /// write partway through a Ctrl/Shift sequence would leave the modifier
    /// permanently stuck down on the virtual device. The first error
    /// encountered (from pressing, `action`, releasing, or syncing) is what
    /// gets returned.
    fn with_modifier<F>(&mut self, modifier: u32, sync_after: bool, action: F) -> Result<()>
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        self.send_key_down(modifier)?;
        thread::sleep(STEP_DELAY);

        let action_result = action(self);

        let release_result = self.send_key_up(modifier);
        thread::sleep(STEP_DELAY);

        let sync_result = if sync_after { self.send_sync() } else { Ok(()) };

        action_result.and(release_result).and(sync_result)
    }

    /// Simulate Ctrl+V (paste).
    pub fn send_ctrl_v(&mut self) -> Result<()> {
        self.with_modifier(KEY_LEFTCTRL, true, |dev| dev.tap_key(KEY_V))
    }

    /// Simulate Ctrl+Shift+V (the paste shortcut in terminals).
    pub fn send_ctrl_shift_v(&mut self) -> Result<()> {
        self.with_two_modifiers(KEY_LEFTCTRL, KEY_LEFTSHIFT, |dev| dev.tap_key(KEY_V))
    }

    /// Simulate Ctrl+C (copy).
    pub fn send_ctrl_c(&mut self) -> Result<()> {
        self.with_modifier(KEY_LEFTCTRL, true, |dev| dev.tap_key(KEY_C))
    }

    /// Simulate Ctrl+Shift+C (the copy shortcut in terminals, where plain
    /// Ctrl+C means SIGINT).
    pub fn send_ctrl_shift_c(&mut self) -> Result<()> {
        self.with_two_modifiers(KEY_LEFTCTRL, KEY_LEFTSHIFT, |dev| dev.tap_key(KEY_C))
    }

    /// Holds both `first` and `second` modifiers down (in that order), runs
    /// `action`, then releases them in reverse. The inner modifier syncs so
    /// applications observe the full chord; releases are always attempted,
    /// mirroring [`Self::with_modifier`].
    fn with_two_modifiers<F>(&mut self, first: u32, second: u32, action: F) -> Result<()>
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        self.with_modifier(first, true, |dev| dev.with_modifier(second, false, action))
    }

    /// Position the cursor at `pos` in the current line and delete the
    /// `tag_len` characters of the tag.
    ///
    /// This avoids the clipboard by:
    /// 1. Moving cursor to start of line (Home)
    /// 2. Moving right `pos` times to reach the target position
    /// 3. Selecting `tag_len` characters (Shift+Right)
    /// 4. Deleting the selection (Backspace)
    pub fn position_at_tag(&mut self, pos: usize, tag_len: usize) -> Result<()> {
        self.tap_key(KEY_HOME)?;

        for _ in 0..pos {
            self.tap_key(KEY_RIGHT)?;
        }

        self.with_modifier(KEY_LEFTSHIFT, false, |dev| {
            for _ in 0..tag_len {
                dev.tap_key(KEY_RIGHT)?;
            }
            Ok(())
        })?;

        self.tap_key(KEY_BACKSPACE)?;
        self.send_sync()
    }

    /// Select the entire current line (Home, then Shift+End).
    pub fn select_line(&mut self) -> Result<()> {
        self.tap_key(KEY_HOME)?;
        self.with_modifier(KEY_LEFTSHIFT, true, |dev| dev.tap_key(KEY_END))
    }

    /// Press and release the left shift key.
    pub fn send_shift_down(&mut self) -> Result<()> {
        self.write_event(&InputEvent::key_event(KEY_LEFTSHIFT, 1))?;
        thread::sleep(STEP_DELAY);
        Ok(())
    }

    /// Release the left shift key.
    pub fn send_shift_up(&mut self) -> Result<()> {
        self.write_event(&InputEvent::key_event(KEY_LEFTSHIFT, 0))?;
        self.send_sync()?;
        thread::sleep(STEP_DELAY);
        Ok(())
    }

    /// Send an ASCII string through the virtual keyboard, typing each
    /// character individually via key events (with shift as needed).
    pub fn send_string(&mut self, string: &str) -> Result<()> {
        for ch in string.chars() {
            let key = get_key_from_char(ch).ok_or(BaanError::InvalidCharacter { ch })?;

            if key.is_shifted {
                self.with_modifier(KEY_LEFTSHIFT, true, |dev| dev.tap_key(key.keycode))?;
            } else {
                self.tap_key(key.keycode)?;
            }
        }
        self.send_sync()
    }

    /// Destroy the virtual device (called on shutdown).
    fn destroy_inner(&mut self) -> Result<()> {
        unsafe {
            ioctl(
                self.device.as_raw_fd(),
                UI_DEV_DESTROY,
                0,
                |detail| BaanError::DestroyVirtualDevice { detail },
                "UI_DEV_DESTROY",
            )
        }
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        // Attempt to destroy the virtual device; ignoring errors because
        // panicking in a destructor is UB.
        let _ = self.destroy_inner();
    }
}

// ---- Physical keyboard device ----
pub struct KeyboardDevice {
    device: File,
    path: PathBuf,
}

#[allow(dead_code)]
impl KeyboardDevice {
    /// Open the physical keyboard device at `path` for reading and writing.
    pub fn open(path: PathBuf) -> Result<Self> {
        let device = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| BaanError::Open {
                path: path.display().to_string(),
                source: e,
            })?;
        info!(path = %path.display(), "Opened keyboard device");
        Ok(Self { device, path })
    }

    /// Read a single `InputEvent` from the device (blocks until available).
    ///
    /// Returns `Ok(None)` when a signal interrupts the read before any bytes
    /// of the next event arrive (e.g. SIGTERM with handlers installed without
    /// `SA_RESTART`). Callers should re-check their terminate flag and loop.
    ///
    /// Partial reads are completed even across interrupts so the 24-byte event
    /// stream stays aligned with the kernel.
    pub fn read_event(&mut self) -> Result<Option<InputEvent>> {
        use std::io::ErrorKind;

        let mut bytes = [0u8; 24];
        let mut filled = 0;

        while filled < bytes.len() {
            match self.device.read(&mut bytes[filled..]) {
                Ok(0) => {
                    return Err(BaanError::Read {
                        path: self.path.display().to_string(),
                        source: std::io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "unexpected EOF reading input event",
                        ),
                    });
                }
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                    if filled == 0 {
                        return Ok(None);
                    }
                    // Mid-event: keep reading so we stay aligned with the kernel stream.
                }
                Err(e) => {
                    return Err(BaanError::Read {
                        path: self.path.display().to_string(),
                        source: e,
                    });
                }
            }
        }

        Ok(Some(event_from_bytes(bytes)))
    }

    /// Write a raw `InputEvent` to the device (low-level).
    pub fn write_event(&mut self, event: &InputEvent) -> Result<()> {
        self.device
            .write_all(event_bytes(event))
            .map_err(|e| BaanError::Write {
                detail: self.path.display().to_string(),
                source: e,
            })
    }

    /// Send a key-up event to the physical keyboard device.
    pub fn send_key_up(&mut self, keycode: u32) -> Result<()> {
        self.write_event(&InputEvent::key_event(keycode, 0))
    }

    /// Send a sync event to the physical keyboard device.
    pub fn send_sync(&mut self) -> Result<()> {
        self.write_event(&InputEvent::sync_event())
    }

    /// Returns the path of the device this instance manages.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl KeyInjector for VirtualDevice {
    fn select_line(&mut self) -> Result<()> {
        VirtualDevice::select_line(self)
    }
    fn send_ctrl_c(&mut self) -> Result<()> {
        VirtualDevice::send_ctrl_c(self)
    }
    fn send_ctrl_shift_c(&mut self) -> Result<()> {
        VirtualDevice::send_ctrl_shift_c(self)
    }
    fn send_ctrl_v(&mut self) -> Result<()> {
        VirtualDevice::send_ctrl_v(self)
    }
    fn send_ctrl_shift_v(&mut self) -> Result<()> {
        VirtualDevice::send_ctrl_shift_v(self)
    }
    fn send_string(&mut self, s: &str) -> Result<()> {
        VirtualDevice::send_string(self, s)
    }
    fn position_at_tag(&mut self, pos: usize, len: usize) -> Result<()> {
        VirtualDevice::position_at_tag(self, pos, len)
    }
    fn send_backspace(&mut self, n: usize) -> Result<()> {
        VirtualDevice::send_backspace(self, n)
    }
}

/// Reinterprets an `InputEvent` as its raw wire bytes for writing to a
/// uinput/evdev device.
///
/// Safety: `InputEvent` is `#[repr(C)]` and its size is checked at compile
/// time (`_INPUT_EVENT_SIZE_CHECK`) to match the kernel's
/// `struct input_event` exactly, so there are no interior padding bytes to
/// worry about reading. This is the single place that unsafe cast happens;
/// both device types below route through it.
fn event_bytes(event: &InputEvent) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            event as *const InputEvent as *const u8,
            std::mem::size_of::<InputEvent>(),
        )
    }
}

/// Reinterprets 24 freshly-read bytes as an `InputEvent`.
///
/// Safety: `bytes` was filled entirely by a successful read, so every byte
/// is initialized; `InputEvent`'s fields are plain integers that accept any
/// bit pattern. `read_unaligned` is used instead of a direct cast/transmute
/// since the buffer isn't guaranteed to satisfy `InputEvent`'s alignment.
fn event_from_bytes(bytes: [u8; 24]) -> InputEvent {
    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const InputEvent) }
}

/// Converts an ioctl return value into a `Result`, attaching the OS's last
/// error when the call failed (indicated by a negative return).
fn check_ioctl_result(
    ret: i32,
    error_kind: impl FnOnce(String) -> BaanError,
    detail: &str,
) -> Result<()> {
    if ret < 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(error_kind(format!("{}: {}", detail, os_err)));
    }
    Ok(())
}

/// Perform an ioctl that returns `0` on success, `-1` on failure.
unsafe fn ioctl(
    fd: i32,
    request: u64,
    arg: u64,
    error_kind: impl FnOnce(String) -> BaanError,
    detail: &str,
) -> Result<()> {
    let ret = unsafe { libc::ioctl(fd, request as libc::c_ulong, arg as libc::c_ulong) };
    check_ioctl_result(ret, error_kind, detail)
}

/// Same for ioctls that take a pointer argument.
unsafe fn ioctl_ptr(
    fd: i32,
    request: u64,
    arg: *const libc::c_void,
    error_kind: impl FnOnce(String) -> BaanError,
    detail: &str,
) -> Result<()> {
    let ret = unsafe { libc::ioctl(fd, request as libc::c_ulong, arg) };
    check_ioctl_result(ret, error_kind, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keycode::{EV_KEY, KEY_A, KEY_SPACE};

    #[test]
    fn event_bytes_roundtrips_through_event_from_bytes() {
        let ev = InputEvent::key_event(KEY_A, 1);
        let bytes: [u8; 24] = event_bytes(&ev).try_into().unwrap();
        let recovered = event_from_bytes(bytes);
        assert_eq!(event_bytes(&recovered), event_bytes(&ev));
    }

    #[test]
    fn sync_event_serializes_to_expected_wire_bytes() {
        let ev = InputEvent::sync_event();
        let bytes = event_bytes(&ev);
        // tv_sec/tv_usec = 0 (16 zero bytes), then EV_SYN=0, SYN_REPORT=0, value=0.
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn key_event_serializes_type_code_value_little_endian() {
        let ev = InputEvent::key_event(KEY_SPACE, 1);
        let bytes = event_bytes(&ev);
        // Header (16 bytes) is zero.
        assert!(bytes[0..16].iter().all(|&b| b == 0));
        // type: u16 = EV_KEY.
        assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), EV_KEY);
        // code: u16 = KEY_SPACE.
        assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), KEY_SPACE as u16);
        // value: i32 = 1.
        assert_eq!(
            i32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            1
        );
    }

    #[test]
    fn ioctl_success_returns_ok() {
        let result = check_ioctl_result(0, |d| BaanError::Ioctl { detail: d }, "UI_DEV_CREATE");
        assert!(result.is_ok());
    }

    #[test]
    fn ioctl_failure_returns_error_with_context() {
        let result = check_ioctl_result(-1, |d| BaanError::Ioctl { detail: d }, "UI_SET_EVBIT");
        match result {
            Err(BaanError::Ioctl { detail }) => assert!(detail.contains("UI_SET_EVBIT")),
            other => panic!("expected Ioctl error, got {other:?}"),
        }
    }

    #[test]
    fn read_event_reads_aligned_event_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("event0");
        let ev = InputEvent::key_event(KEY_A, 1);
        std::fs::write(&path, event_bytes(&ev)).unwrap();

        let mut kbd = KeyboardDevice::open(path).unwrap();
        let read = kbd.read_event().unwrap().expect("expected one event");
        assert_eq!(event_bytes(&read), event_bytes(&ev));
    }

    #[test]
    fn read_event_errors_on_eof_before_full_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("event-empty");
        std::fs::write(&path, [0u8; 8]).unwrap(); // short of 24 bytes

        let mut kbd = KeyboardDevice::open(path).unwrap();
        assert!(kbd.read_event().is_err());
    }

    #[test]
    fn event_bytes_length_is_exactly_24() {
        let ev = InputEvent::sync_event();
        assert_eq!(event_bytes(&ev).len(), 24);
    }
}
