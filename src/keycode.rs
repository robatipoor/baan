#![allow(dead_code)]
// ---------------------------------------------------------------------------
// Linux input-event-codes constants (from <linux/input-event-codes.h>)
// ---------------------------------------------------------------------------
pub const KEY_ESC: u32 = 1;
pub const KEY_1: u32 = 2;
pub const KEY_2: u32 = 3;
pub const KEY_3: u32 = 4;
pub const KEY_4: u32 = 5;
pub const KEY_5: u32 = 6;
pub const KEY_6: u32 = 7;
pub const KEY_7: u32 = 8;
pub const KEY_8: u32 = 9;
pub const KEY_9: u32 = 10;
pub const KEY_0: u32 = 11;
pub const KEY_MINUS: u32 = 12;
pub const KEY_EQUAL: u32 = 13;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_Q: u32 = 16;
pub const KEY_W: u32 = 17;
pub const KEY_E: u32 = 18;
pub const KEY_R: u32 = 19;
pub const KEY_T: u32 = 20;
pub const KEY_Y: u32 = 21;
pub const KEY_U: u32 = 22;
pub const KEY_I: u32 = 23;
pub const KEY_O: u32 = 24;
pub const KEY_P: u32 = 25;
pub const KEY_LEFTBRACE: u32 = 26;
pub const KEY_RIGHTBRACE: u32 = 27;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_RIGHTCTRL: u32 = 97;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_A: u32 = 30;
pub const KEY_S: u32 = 31;
pub const KEY_D: u32 = 32;
pub const KEY_F: u32 = 33;
pub const KEY_G: u32 = 34;
pub const KEY_H: u32 = 35;
pub const KEY_J: u32 = 36;
pub const KEY_K: u32 = 37;
pub const KEY_L: u32 = 38;
pub const KEY_SEMICOLON: u32 = 39;
pub const KEY_APOSTROPHE: u32 = 40;
pub const KEY_GRAVE: u32 = 41;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_BACKSLASH: u32 = 43;
pub const KEY_Z: u32 = 44;
pub const KEY_X: u32 = 45;
pub const KEY_C: u32 = 46;
pub const KEY_V: u32 = 47;
pub const KEY_B: u32 = 48;
pub const KEY_N: u32 = 49;
pub const KEY_M: u32 = 50;
pub const KEY_COMMA: u32 = 51;
pub const KEY_DOT: u32 = 52;
pub const KEY_SLASH: u32 = 53;
pub const KEY_SPACE: u32 = 57;
pub const KEY_HOME: u32 = 102;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_END: u32 = 107;

// Event type constants
pub const EV_KEY: u16 = 0x01;
pub const EV_SYN: u16 = 0x00;
pub const SYN_REPORT: u32 = 0;
pub const BUS_USB: u16 = 0x03;

// uinput ioctl constants
pub const UI_SET_EVBIT: u64 = 0x40045564;
pub const UI_SET_KEYBIT: u64 = 0x40045565;
pub const UI_DEV_SETUP: u64 = 0x405c5503;
pub const UI_DEV_CREATE: u64 = 0x5501;
pub const UI_DEV_DESTROY: u64 = 0x5502;

/// Sleep time between key events (microseconds).
pub const SLEEP_TIME_US: u64 = 1000;

/// Bit flag indicating the shift key must be held to produce this character.
/// Stored in the high bit so it can be OR'd into a keycode for table indexing.
const FLAG_SHIFT: u32 = 1 << 7; // = 128

/// Size of the keycode lookup tables (covers keycode + shift flag in high bit).
const KEYCODE_TABLE_SIZE: usize = 256;

/// The set of Linux keycodes that baan recognises.
pub const LINUX_KEYS: [u32; 55] = [
    KEY_1,
    KEY_2,
    KEY_3,
    KEY_4,
    KEY_5,
    KEY_6,
    KEY_7,
    KEY_8,
    KEY_9,
    KEY_0,
    KEY_MINUS,
    KEY_EQUAL,
    KEY_BACKSPACE,
    KEY_Q,
    KEY_W,
    KEY_E,
    KEY_R,
    KEY_T,
    KEY_Y,
    KEY_U,
    KEY_I,
    KEY_O,
    KEY_P,
    KEY_LEFTBRACE,
    KEY_RIGHTBRACE,
    KEY_LEFTCTRL,
    KEY_A,
    KEY_S,
    KEY_D,
    KEY_F,
    KEY_G,
    KEY_H,
    KEY_J,
    KEY_K,
    KEY_L,
    KEY_SEMICOLON,
    KEY_APOSTROPHE,
    KEY_GRAVE,
    KEY_BACKSLASH,
    KEY_Z,
    KEY_X,
    KEY_C,
    KEY_V,
    KEY_B,
    KEY_N,
    KEY_M,
    KEY_COMMA,
    KEY_DOT,
    KEY_SLASH,
    KEY_SPACE,
    KEY_LEFTSHIFT,
    KEY_RIGHTSHIFT,
    KEY_RIGHT,
    KEY_HOME,
    KEY_END,
];

//
// Both lookup tables are derived from this at compile time, so they can never
// drift out of sync with each other.

/// `(character, keycode, shift_required)` for every printable ASCII key.
const CHAR_KEYCODE_MAP: &[(char, u32, bool)] = &[
    (' ', KEY_SPACE, false),
    ('!', KEY_1, true),
    ('"', KEY_APOSTROPHE, true),
    ('#', KEY_3, true),
    ('$', KEY_4, true),
    ('%', KEY_5, true),
    ('&', KEY_7, true),
    ('\'', KEY_APOSTROPHE, false),
    ('(', KEY_9, true),
    (')', KEY_0, true),
    ('*', KEY_8, true),
    ('+', KEY_EQUAL, true),
    (',', KEY_COMMA, false),
    ('-', KEY_MINUS, false),
    ('.', KEY_DOT, false),
    ('/', KEY_SLASH, false),
    ('0', KEY_0, false),
    ('1', KEY_1, false),
    ('2', KEY_2, false),
    ('3', KEY_3, false),
    ('4', KEY_4, false),
    ('5', KEY_5, false),
    ('6', KEY_6, false),
    ('7', KEY_7, false),
    ('8', KEY_8, false),
    ('9', KEY_9, false),
    (':', KEY_SEMICOLON, true),
    (';', KEY_SEMICOLON, false),
    ('<', KEY_COMMA, true),
    ('=', KEY_EQUAL, false),
    ('>', KEY_DOT, true),
    ('?', KEY_SLASH, true),
    ('@', KEY_2, true),
    ('A', KEY_A, true),
    ('B', KEY_B, true),
    ('C', KEY_C, true),
    ('D', KEY_D, true),
    ('E', KEY_E, true),
    ('F', KEY_F, true),
    ('G', KEY_G, true),
    ('H', KEY_H, true),
    ('I', KEY_I, true),
    ('J', KEY_J, true),
    ('K', KEY_K, true),
    ('L', KEY_L, true),
    ('M', KEY_M, true),
    ('N', KEY_N, true),
    ('O', KEY_O, true),
    ('P', KEY_P, true),
    ('Q', KEY_Q, true),
    ('R', KEY_R, true),
    ('S', KEY_S, true),
    ('T', KEY_T, true),
    ('U', KEY_U, true),
    ('V', KEY_V, true),
    ('W', KEY_W, true),
    ('X', KEY_X, true),
    ('Y', KEY_Y, true),
    ('Z', KEY_Z, true),
    ('[', KEY_LEFTBRACE, false),
    ('\\', KEY_BACKSLASH, false),
    (']', KEY_RIGHTBRACE, false),
    ('^', KEY_6, true),
    ('_', KEY_MINUS, true),
    ('`', KEY_GRAVE, false),
    ('a', KEY_A, false),
    ('b', KEY_B, false),
    ('c', KEY_C, false),
    ('d', KEY_D, false),
    ('e', KEY_E, false),
    ('f', KEY_F, false),
    ('g', KEY_G, false),
    ('h', KEY_H, false),
    ('i', KEY_I, false),
    ('j', KEY_J, false),
    ('k', KEY_K, false),
    ('l', KEY_L, false),
    ('m', KEY_M, false),
    ('n', KEY_N, false),
    ('o', KEY_O, false),
    ('p', KEY_P, false),
    ('q', KEY_Q, false),
    ('r', KEY_R, false),
    ('s', KEY_S, false),
    ('t', KEY_T, false),
    ('u', KEY_U, false),
    ('v', KEY_V, false),
    ('w', KEY_W, false),
    ('x', KEY_X, false),
    ('y', KEY_Y, false),
    ('z', KEY_Z, false),
    ('{', KEY_LEFTBRACE, true),
    ('|', KEY_BACKSLASH, true),
    ('}', KEY_RIGHTBRACE, true),
    ('~', KEY_GRAVE, true),
];

/// Maps ASCII value (index = char as usize) → (keycode, shift_required).
/// Valid range is 0x20 (' ') through 0x7E ('~').
static CHAR_TO_KEYCODE: [Option<(u32, bool)>; 128] = build_char_to_keycode();

/// Maps `keycode | (shift ? FLAG_SHIFT : 0)` → the character it produces.
static KEYCODE_TO_CHAR: [Option<char>; KEYCODE_TABLE_SIZE] = build_keycode_to_char();

const fn build_char_to_keycode() -> [Option<(u32, bool)>; 128] {
    let mut arr = [None; 128];
    let mut i = 0;
    while i < CHAR_KEYCODE_MAP.len() {
        let (ch, kc, shifted) = CHAR_KEYCODE_MAP[i];
        let idx = ch as usize;
        // Guard: only write into the array if the char fits.
        if idx < 128 {
            arr[idx] = Some((kc, shifted));
        }
        i += 1;
    }
    arr
}

const fn build_keycode_to_char() -> [Option<char>; KEYCODE_TABLE_SIZE] {
    let mut arr = [None; KEYCODE_TABLE_SIZE];
    let mut i = 0;
    while i < CHAR_KEYCODE_MAP.len() {
        let (ch, kc, shifted) = CHAR_KEYCODE_MAP[i];
        let idx = (kc | if shifted { FLAG_SHIFT } else { 0 }) as usize;
        if idx < KEYCODE_TABLE_SIZE {
            arr[idx] = Some(ch);
        }
        i += 1;
    }
    arr
}

/// A resolved key: its Linux keycode, whether shift must be held, and the
/// character it produces. The `position` is the character's ASCII value, used
/// as the index into a trie node's children array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// ASCII value of the character — used as the trie children index.
    pub position: usize,
    pub character: char,
    pub keycode: u32,
    pub is_shifted: bool,
}

/// Returns `true` if the given Linux keycode is one baan recognises
/// (with or without shift).
pub fn is_supported_key_code(code: u32) -> bool {
    get_char_from_keycode(code, false).is_some() || get_char_from_keycode(code, true).is_some()
}

/// Look up the `Key` for a given ASCII character.
/// Returns `None` if the character has no keycode mapping.
pub fn get_key_from_char(character: char) -> Option<Key> {
    let idx = character as usize;
    if idx >= 128 {
        return None;
    }
    let (keycode, is_shifted) = CHAR_TO_KEYCODE[idx]?;
    Some(Key {
        position: idx,
        character,
        keycode,
        is_shifted,
    })
}

/// Get the character that a (keycode, shift) pair produces.
pub fn get_char_from_keycode(keycode: u32, is_shifted: bool) -> Option<char> {
    let idx = (keycode | if is_shifted { FLAG_SHIFT } else { 0 }) as usize;
    KEYCODE_TO_CHAR.get(idx).copied().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_from_char_lowercase() {
        let key = get_key_from_char('a').unwrap();
        assert_eq!(key.keycode, KEY_A);
        assert!(!key.is_shifted);
        assert_eq!(key.position, 'a' as usize);
    }

    #[test]
    fn test_key_from_char_uppercase() {
        let key = get_key_from_char('A').unwrap();
        assert_eq!(key.keycode, KEY_A);
        assert!(key.is_shifted);
        assert_eq!(key.position, 'A' as usize);
    }

    #[test]
    fn test_key_from_char_digit() {
        let key = get_key_from_char('5').unwrap();
        assert_eq!(key.keycode, KEY_5);
        assert!(!key.is_shifted);
    }

    #[test]
    fn test_key_from_char_symbol() {
        let key = get_key_from_char('!').unwrap();
        assert_eq!(key.keycode, KEY_1);
        assert!(key.is_shifted);
    }

    #[test]
    fn test_key_from_char_space() {
        let key = get_key_from_char(' ').unwrap();
        assert_eq!(key.keycode, KEY_SPACE);
        assert!(!key.is_shifted);
    }

    #[test]
    fn test_key_from_char_unsupported() {
        assert!(get_key_from_char('\0').is_none());
        assert!(get_key_from_char('\n').is_none());
        assert!(get_key_from_char('\t').is_none());
    }

    #[test]
    fn test_is_supported_key_code() {
        assert!(is_supported_key_code(KEY_A));
        assert!(is_supported_key_code(KEY_SPACE));
        assert!(is_supported_key_code(KEY_1));
        assert!(!is_supported_key_code(KEY_ESC));
        assert!(!is_supported_key_code(KEY_ENTER));
    }

    #[test]
    fn test_char_keycode_roundtrip() {
        // Every entry in the map should round-trip through both tables.
        for &(ch, kc, shifted) in super::CHAR_KEYCODE_MAP {
            let key = get_key_from_char(ch).expect("char should be in table");
            assert_eq!(key.keycode, kc, "keycode mismatch for '{ch}'");
            assert_eq!(key.is_shifted, shifted, "shift mismatch for '{ch}'");

            let recovered =
                get_char_from_keycode(kc, shifted).expect("keycode+shift should round-trip");
            assert_eq!(
                recovered, ch,
                "char mismatch for keycode {kc} shifted={shifted}"
            );
        }
    }
}
