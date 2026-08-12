use crate::keycode::{EV_KEY, EV_SYN, SYN_REPORT};

// ---------------------------------------------------------------------------
// Linux input structures (used for reading/writing /dev/input and uinput)
// ---------------------------------------------------------------------------

/// Represents `struct input_event` from `<linux/input.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEvent {
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub r#type: u16,
    pub code: u16,
    pub value: i32,
}

impl InputEvent {
    pub fn key_event(code: u32, value: i32) -> Self {
        Self {
            tv_sec: 0,
            tv_usec: 0,
            r#type: EV_KEY,
            code: code.try_into().unwrap(),
            value,
        }
    }

    pub fn sync_event() -> Self {
        Self {
            tv_sec: 0,
            tv_usec: 0,
            r#type: EV_SYN,
            code: SYN_REPORT as u16,
            value: 0,
        }
    }
}

/// Represents `struct uinput_setup` from `<linux/uinput.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UinputSetup {
    pub id: UinputId,
    pub name: [u8; 80],
    pub ff_effects_max: u32,
}

impl Default for UinputSetup {
    fn default() -> Self {
        Self {
            id: UinputId::default(),
            name: [0u8; 80],
            ff_effects_max: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct UinputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

impl UinputSetup {
    pub fn new(name: &str, bustype: u16, vendor: u16, product: u16) -> Self {
        let mut setup = Self::default();
        setup.id.bustype = bustype;
        setup.id.vendor = vendor;
        setup.id.product = product;
        let name_bytes = name.as_bytes();
        let len = name_bytes.len().min(79);
        setup.name[..len].copy_from_slice(&name_bytes[..len]);
        setup.name[len] = 0; // null-terminate
        setup
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keycode::{EV_KEY, EV_SYN, KEY_A, KEY_SPACE, SYN_REPORT};

    // Composite structs with no padding bytes: a wrong size would desync the
    // raw event stream from the kernel.
    #[test]
    fn input_event_has_expected_kernel_size() {
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        // UinputId(8, align 2) + name[80] + ff_effects_max u32(4) = 92, pad to align 4.
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputId>(), 8);
    }

    #[test]
    fn key_event_sets_key_fields() {
        let ev = InputEvent::key_event(KEY_A, 1);
        assert_eq!(ev.r#type, EV_KEY);
        assert_eq!(ev.code, KEY_A as u16);
        assert_eq!(ev.value, 1);
        assert_eq!(ev.tv_sec, 0);
        assert_eq!(ev.tv_usec, 0);
    }

    #[test]
    fn key_event_release_value_is_kept() {
        let ev = InputEvent::key_event(KEY_SPACE, 0);
        assert_eq!(ev.r#type, EV_KEY);
        assert_eq!(ev.code, KEY_SPACE as u16);
        assert_eq!(ev.value, 0);
    }

    #[test]
    fn sync_event_sets_sync_fields() {
        let ev = InputEvent::sync_event();
        assert_eq!(ev.r#type, EV_SYN);
        assert_eq!(ev.code, SYN_REPORT as u16);
        assert_eq!(ev.value, 0);
    }

    #[test]
    fn uinput_setup_sets_ids() {
        let setup = UinputSetup::new("baan", 3, 1187, 1999);
        assert_eq!(setup.id.bustype, 3);
        assert_eq!(setup.id.vendor, 1187);
        assert_eq!(setup.id.product, 1999);
        assert_eq!(setup.ff_effects_max, 0);
    }

    #[test]
    fn uinput_setup_short_name_is_padded_and_terminated() {
        let setup = UinputSetup::new("baan", 0, 0, 0);
        assert_eq!(&setup.name[..5], b"baan\0");
        assert!(setup.name[..80].iter().any(|&b| b != 0)); // has data
        assert_eq!(setup.name[4], 0); // null-terminated
    }

    #[test]
    fn uinput_setup_long_name_is_truncated_and_terminated() {
        let long = "x".repeat(200);
        let setup = UinputSetup::new(&long, 0, 0, 0);
        // 79 chars of content then a trailing NUL fill the 80-byte field.
        assert_eq!(setup.name[0], b'x');
        assert_eq!(setup.name[78], b'x');
        assert_eq!(setup.name[79], 0);
    }

    #[test]
    fn uinput_setup_name_field_zero_initialized_after_name() {
        let setup = UinputSetup::new("ab", 0, 0, 0);
        assert_eq!(&setup.name[..3], b"ab\0");
        assert_eq!(setup.name[3], 0);
    }
}
