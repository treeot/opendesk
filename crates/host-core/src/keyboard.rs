//! Layout-aware keyboard injection: `InputEvent::ScanKey` presses a physical
//! key by its USB HID usage, so the host's own layout picks the character,
//! and `InputEvent::TypeChar` types a character whatever that layout is.
//!
//! Keys pressed here are remembered so a session that ends mid-press (the
//! viewer vanished, the network dropped) can release them instead of leaving
//! Shift or Ctrl stuck down on the host.

use enigo::{Direction, Enigo, Key, Keyboard};

/// Marks a key with no native code on that platform.
const NONE: u16 = u16::MAX;

/// One physical key: its USB HID usage (page << 16 | id), the Windows set 1
/// scan code (0xE0 prefix in the high byte for extended keys), the macOS
/// virtual key code and the Linux evdev code (X11 keycodes add 8).
struct KeyCodes {
    usage: u32,
    windows: u16,
    mac: u16,
    evdev: u16,
}

const fn k(usage: u32, windows: u16, mac: u16, evdev: u16) -> KeyCodes {
    KeyCodes { usage, windows, mac, evdev }
}

/// The keys a viewer can send, by HID usage. Matches the DOM `code` table in
/// viewer/src/lib/keyboard.ts; the codes follow Chromium's dom_code_data.inc.
#[rustfmt::skip]
const KEYS: &[KeyCodes] = &[
    // Letters.
    k(0x07_0004, 0x1E, 0x00, 30), // KeyA
    k(0x07_0005, 0x30, 0x0B, 48), // KeyB
    k(0x07_0006, 0x2E, 0x08, 46), // KeyC
    k(0x07_0007, 0x20, 0x02, 32), // KeyD
    k(0x07_0008, 0x12, 0x0E, 18), // KeyE
    k(0x07_0009, 0x21, 0x03, 33), // KeyF
    k(0x07_000A, 0x22, 0x05, 34), // KeyG
    k(0x07_000B, 0x23, 0x04, 35), // KeyH
    k(0x07_000C, 0x17, 0x22, 23), // KeyI
    k(0x07_000D, 0x24, 0x26, 36), // KeyJ
    k(0x07_000E, 0x25, 0x28, 37), // KeyK
    k(0x07_000F, 0x26, 0x25, 38), // KeyL
    k(0x07_0010, 0x32, 0x2E, 50), // KeyM
    k(0x07_0011, 0x31, 0x2D, 49), // KeyN
    k(0x07_0012, 0x18, 0x1F, 24), // KeyO
    k(0x07_0013, 0x19, 0x23, 25), // KeyP
    k(0x07_0014, 0x10, 0x0C, 16), // KeyQ
    k(0x07_0015, 0x13, 0x0F, 19), // KeyR
    k(0x07_0016, 0x1F, 0x01, 31), // KeyS
    k(0x07_0017, 0x14, 0x11, 20), // KeyT
    k(0x07_0018, 0x16, 0x20, 22), // KeyU
    k(0x07_0019, 0x2F, 0x09, 47), // KeyV
    k(0x07_001A, 0x11, 0x0D, 17), // KeyW
    k(0x07_001B, 0x2D, 0x07, 45), // KeyX
    k(0x07_001C, 0x15, 0x10, 21), // KeyY
    k(0x07_001D, 0x2C, 0x06, 44), // KeyZ
    // Digit row.
    k(0x07_001E, 0x02, 0x12, 2),  // Digit1
    k(0x07_001F, 0x03, 0x13, 3),  // Digit2
    k(0x07_0020, 0x04, 0x14, 4),  // Digit3
    k(0x07_0021, 0x05, 0x15, 5),  // Digit4
    k(0x07_0022, 0x06, 0x17, 6),  // Digit5
    k(0x07_0023, 0x07, 0x16, 7),  // Digit6
    k(0x07_0024, 0x08, 0x1A, 8),  // Digit7
    k(0x07_0025, 0x09, 0x1C, 9),  // Digit8
    k(0x07_0026, 0x0A, 0x19, 10), // Digit9
    k(0x07_0027, 0x0B, 0x1D, 11), // Digit0
    // Editing and punctuation.
    k(0x07_0028, 0x1C, 0x24, 28), // Enter
    k(0x07_0029, 0x01, 0x35, 1),  // Escape
    k(0x07_002A, 0x0E, 0x33, 14), // Backspace
    k(0x07_002B, 0x0F, 0x30, 15), // Tab
    k(0x07_002C, 0x39, 0x31, 57), // Space
    k(0x07_002D, 0x0C, 0x1B, 12), // Minus
    k(0x07_002E, 0x0D, 0x18, 13), // Equal
    k(0x07_002F, 0x1A, 0x21, 26), // BracketLeft
    k(0x07_0030, 0x1B, 0x1E, 27), // BracketRight
    k(0x07_0031, 0x2B, 0x2A, 43), // Backslash
    k(0x07_0033, 0x27, 0x29, 39), // Semicolon
    k(0x07_0034, 0x28, 0x27, 40), // Quote
    k(0x07_0035, 0x29, 0x32, 41), // Backquote
    k(0x07_0036, 0x33, 0x2B, 51), // Comma
    k(0x07_0037, 0x34, 0x2F, 52), // Period
    k(0x07_0038, 0x35, 0x2C, 53), // Slash
    k(0x07_0039, 0x3A, 0x39, 58), // CapsLock
    // Function row.
    k(0x07_003A, 0x3B, 0x7A, 59), // F1
    k(0x07_003B, 0x3C, 0x78, 60), // F2
    k(0x07_003C, 0x3D, 0x63, 61), // F3
    k(0x07_003D, 0x3E, 0x76, 62), // F4
    k(0x07_003E, 0x3F, 0x60, 63), // F5
    k(0x07_003F, 0x40, 0x61, 64), // F6
    k(0x07_0040, 0x41, 0x62, 65), // F7
    k(0x07_0041, 0x42, 0x64, 66), // F8
    k(0x07_0042, 0x43, 0x65, 67), // F9
    k(0x07_0043, 0x44, 0x6D, 68), // F10
    k(0x07_0044, 0x57, 0x67, 87), // F11
    k(0x07_0045, 0x58, 0x6F, 88), // F12
    k(0x07_0046, 0xE037, NONE, 99),  // PrintScreen
    k(0x07_0047, 0x46, NONE, 70),    // ScrollLock
    k(0x07_0048, NONE, NONE, 119),   // Pause
    // Navigation.
    k(0x07_0049, 0xE052, 0x72, 110), // Insert (Help on a Mac)
    k(0x07_004A, 0xE047, 0x73, 102), // Home
    k(0x07_004B, 0xE049, 0x74, 104), // PageUp
    k(0x07_004C, 0xE053, 0x75, 111), // Delete
    k(0x07_004D, 0xE04F, 0x77, 107), // End
    k(0x07_004E, 0xE051, 0x79, 109), // PageDown
    k(0x07_004F, 0xE04D, 0x7C, 106), // ArrowRight
    k(0x07_0050, 0xE04B, 0x7B, 105), // ArrowLeft
    k(0x07_0051, 0xE050, 0x7D, 108), // ArrowDown
    k(0x07_0052, 0xE048, 0x7E, 103), // ArrowUp
    // Numpad.
    k(0x07_0053, 0xE045, 0x47, 69),  // NumLock (Clear on a Mac)
    k(0x07_0054, 0xE035, 0x4B, 98),  // NumpadDivide
    k(0x07_0055, 0x37, 0x43, 55),    // NumpadMultiply
    k(0x07_0056, 0x4A, 0x4E, 74),    // NumpadSubtract
    k(0x07_0057, 0x4E, 0x45, 78),    // NumpadAdd
    k(0x07_0058, 0xE01C, 0x4C, 96),  // NumpadEnter
    k(0x07_0059, 0x4F, 0x53, 79),    // Numpad1
    k(0x07_005A, 0x50, 0x54, 80),    // Numpad2
    k(0x07_005B, 0x51, 0x55, 81),    // Numpad3
    k(0x07_005C, 0x4B, 0x56, 75),    // Numpad4
    k(0x07_005D, 0x4C, 0x57, 76),    // Numpad5
    k(0x07_005E, 0x4D, 0x58, 77),    // Numpad6
    k(0x07_005F, 0x47, 0x59, 71),    // Numpad7
    k(0x07_0060, 0x48, 0x5B, 72),    // Numpad8
    k(0x07_0061, 0x49, 0x5C, 73),    // Numpad9
    k(0x07_0062, 0x52, 0x52, 82),    // Numpad0
    k(0x07_0063, 0x53, 0x41, 83),    // NumpadDecimal
    k(0x07_0064, 0x56, 0x0A, 86),    // IntlBackslash (the extra ISO key)
    k(0x07_0065, 0xE05D, 0x6E, 127), // ContextMenu
    k(0x07_0067, 0x59, 0x51, 117),   // NumpadEqual
    // F13 and up.
    k(0x07_0068, 0x64, 0x69, 183), // F13
    k(0x07_0069, 0x65, 0x6B, 184), // F14
    k(0x07_006A, 0x66, 0x71, 185), // F15
    k(0x07_006B, 0x67, 0x6A, 186), // F16
    k(0x07_006C, 0x68, 0x40, 187), // F17
    k(0x07_006D, 0x69, 0x4F, 188), // F18
    k(0x07_006E, 0x6A, 0x50, 189), // F19
    k(0x07_006F, 0x6B, 0x5A, 190), // F20
    k(0x07_0070, 0x6C, NONE, 191), // F21
    k(0x07_0071, 0x6D, NONE, 192), // F22
    k(0x07_0072, 0x6E, NONE, 193), // F23
    k(0x07_0073, 0x76, NONE, 194), // F24
    // Volume.
    k(0x07_007F, 0xE020, 0x4A, 113), // AudioVolumeMute
    k(0x07_0080, 0xE030, 0x48, 115), // AudioVolumeUp
    k(0x07_0081, 0xE02E, 0x49, 114), // AudioVolumeDown
    // Japanese and Korean keys.
    k(0x07_0087, 0x73, 0x5E, 89),   // IntlRo
    k(0x07_0088, 0x70, 0x68, 93),   // KanaMode
    k(0x07_0089, 0x7D, 0x5D, 124),  // IntlYen
    k(0x07_008A, 0x79, NONE, 92),   // Convert
    k(0x07_008B, 0x7B, NONE, 94),   // NonConvert
    k(0x07_0090, 0x72, 0x68, 122),  // Lang1
    k(0x07_0091, 0x71, 0x66, 123),  // Lang2
    // Modifiers.
    k(0x07_00E0, 0x1D, 0x3B, 29),    // ControlLeft
    k(0x07_00E1, 0x2A, 0x38, 42),    // ShiftLeft
    k(0x07_00E2, 0x38, 0x3A, 56),    // AltLeft
    k(0x07_00E3, 0xE05B, 0x37, 125), // MetaLeft
    k(0x07_00E4, 0xE01D, 0x3E, 97),  // ControlRight
    k(0x07_00E5, 0x36, 0x3C, 54),    // ShiftRight
    k(0x07_00E6, 0xE038, 0x3D, 100), // AltRight
    k(0x07_00E7, 0xE05C, 0x36, 126), // MetaRight
    // Media (consumer page).
    k(0x0C_00B5, 0xE019, NONE, 163), // MediaTrackNext
    k(0x0C_00B6, 0xE010, NONE, 165), // MediaTrackPrevious
    k(0x0C_00B7, 0xE024, NONE, 166), // MediaStop
    k(0x0C_00CD, 0xE022, NONE, 164), // MediaPlayPause
];

/// The native key code `usage` has on `platform`, if the platform has the key.
fn native_code_for(usage: u32, platform: Platform) -> Option<u16> {
    let key = KEYS.iter().find(|key| key.usage == usage)?;
    let code = match platform {
        Platform::Windows => key.windows,
        Platform::Mac => key.mac,
        Platform::Linux => key.evdev.checked_add(8)?,
    };
    (code != NONE).then_some(code)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    Mac,
    Linux,
}

const PLATFORM: Platform = if cfg!(target_os = "windows") {
    Platform::Windows
} else if cfg!(target_os = "macos") {
    Platform::Mac
} else {
    Platform::Linux
};

/// The code this host's OS knows `usage` by: a scan code on Windows, a
/// virtual key code on macOS, an X11 keycode on Linux.
pub fn native_code(usage: u32) -> Option<u16> {
    native_code_for(usage, PLATFORM)
}

/// HID usages of the modifier keys, with their `InputEvent` modifier bit.
const MODIFIER_USAGES: [(u32, u8); 8] = [
    (0x07_00E0, 1 << 1),
    (0x07_00E1, 1),
    (0x07_00E2, 1 << 2),
    (0x07_00E3, 1 << 3),
    (0x07_00E4, 1 << 1),
    (0x07_00E5, 1),
    (0x07_00E6, 1 << 2),
    (0x07_00E7, 1 << 3),
];

/// The modifiers to hold around `usage`: the bits in `modifiers`, minus the
/// key's own bit when it is a modifier itself.
fn held_modifiers(usage: u32, modifiers: u8) -> Vec<Key> {
    let own = MODIFIER_USAGES.iter().find(|(u, _)| *u == usage).map_or(0, |(_, bit)| *bit);
    [(1, Key::Shift), (1 << 1, Key::Control), (1 << 2, Key::Alt), (1 << 3, Key::Meta)]
        .into_iter()
        .filter(|(bit, _)| modifiers & bit != 0 && own & bit == 0)
        .map(|(_, key)| key)
        .collect()
}

/// Native key codes pressed and not yet released, oldest first.
#[derive(Default, Debug)]
pub struct HeldKeys {
    codes: Vec<u16>,
}

impl HeldKeys {
    fn press(&mut self, code: u16) {
        if !self.codes.contains(&code) {
            self.codes.push(code);
        }
    }

    fn release(&mut self, code: u16) {
        self.codes.retain(|held| *held != code);
    }

    /// Everything still down, newest first, forgetting it all.
    fn drain(&mut self) -> Vec<u16> {
        let mut codes = std::mem::take(&mut self.codes);
        codes.reverse();
        codes
    }
}

/// Presses or releases the physical key `usage`, holding `modifiers` around a
/// press. Keys this host has no code for are dropped.
pub fn scan_key(enigo: &mut Enigo, held: &mut HeldKeys, usage: u32, down: bool, modifiers: u8) {
    let Some(code) = native_code(usage) else { return };
    let modifiers = held_modifiers(usage, modifiers);
    if down {
        for modifier in &modifiers {
            let _ = enigo.key(*modifier, Direction::Press);
        }
        native_key(enigo, code, true);
        held.press(code);
    } else {
        native_key(enigo, code, false);
        held.release(code);
        for modifier in modifiers.iter().rev() {
            let _ = enigo.key(*modifier, Direction::Release);
        }
    }
}

/// Types `ch` as text, independent of the host's layout.
pub fn type_char(enigo: &mut Enigo, ch: char) {
    if ch.is_control() {
        return;
    }
    let _ = enigo.text(ch.encode_utf8(&mut [0; 4]));
}

/// Releases every key `scan_key` still holds down.
pub fn release_all(enigo: &mut Enigo, held: &mut HeldKeys) {
    for code in held.drain() {
        native_key(enigo, code, false);
    }
}

/// Windows: a scan code through `SendInput`, so the host's layout decides
/// what the key means. Enigo's raw path drops the 0xE0 prefix that tells the
/// arrows and the navigation block apart from the numpad.
#[cfg(target_os = "windows")]
fn native_key(_enigo: &mut Enigo, code: u16, down: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_SCANCODE, VIRTUAL_KEY,
    };
    let mut flags = KEYEVENTF_SCANCODE;
    if code & 0xFF00 == 0xE000 {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: code & 0xFF, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    // SAFETY: one fully initialized INPUT with its correct size.
    unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
}

/// macOS (virtual key codes) and X11 (keycodes): enigo's raw key path.
#[cfg(not(target_os = "windows"))]
fn native_key(enigo: &mut Enigo, code: u16, down: bool) {
    let _ = enigo.raw(code, if down { Direction::Press } else { Direction::Release });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usages_are_unique() {
        for (i, key) in KEYS.iter().enumerate() {
            assert!(KEYS[i + 1..].iter().all(|other| other.usage != key.usage), "duplicate usage {:#x}", key.usage);
        }
    }

    #[test]
    fn native_codes_are_unique_per_platform() {
        // Two usages may share a Mac code (Lang1 is the Kana key there); no
        // other platform may map two keys to one code.
        for platform in [Platform::Windows, Platform::Linux] {
            let codes: Vec<u16> = KEYS.iter().filter_map(|key| native_code_for(key.usage, platform)).collect();
            for (i, code) in codes.iter().enumerate() {
                assert!(!codes[i + 1..].contains(code), "{platform:?} code {code:#x} used twice");
            }
        }
    }

    #[test]
    fn letters_and_layout_keys_map_per_platform() {
        // KeyA, KeyZ (Y on a German keyboard), KeyQ (A on a French one).
        assert_eq!(native_code_for(0x07_0004, Platform::Windows), Some(0x1E));
        assert_eq!(native_code_for(0x07_0004, Platform::Mac), Some(0x00));
        assert_eq!(native_code_for(0x07_0004, Platform::Linux), Some(38));
        assert_eq!(native_code_for(0x07_001D, Platform::Mac), Some(0x06));
        assert_eq!(native_code_for(0x07_0014, Platform::Linux), Some(24));
        // The ISO key next to left Shift (< > on German keyboards).
        assert_eq!(native_code_for(0x07_0064, Platform::Windows), Some(0x56));
        assert_eq!(native_code_for(0x07_0064, Platform::Mac), Some(0x0A));
    }

    #[test]
    fn extended_keys_keep_their_prefix_on_windows() {
        assert_eq!(native_code_for(0x07_0052, Platform::Windows), Some(0xE048)); // ArrowUp
        assert_eq!(native_code_for(0x07_0060, Platform::Windows), Some(0x48)); // Numpad8
        assert_eq!(native_code_for(0x07_00E6, Platform::Windows), Some(0xE038)); // AltRight (AltGr)
    }

    #[test]
    fn missing_keys_have_no_code() {
        assert_eq!(native_code_for(0x07_0046, Platform::Mac), None); // PrintScreen
        assert_eq!(native_code_for(0x07_0048, Platform::Windows), None); // Pause
        assert_eq!(native_code_for(0xDEAD_BEEF, Platform::Linux), None);
    }

    #[test]
    fn modifiers_skip_the_key_itself() {
        assert_eq!(held_modifiers(0x07_0004, 0b1011), vec![Key::Shift, Key::Control, Key::Meta]);
        // ShiftLeft with the shift bit set: Shift is the key, not a modifier.
        assert_eq!(held_modifiers(0x07_00E1, 0b0011), vec![Key::Control]);
        assert!(held_modifiers(0x07_0028, 0).is_empty());
    }

    #[test]
    fn held_keys_release_newest_first_once() {
        let mut held = HeldKeys::default();
        held.press(1);
        held.press(2);
        held.press(2);
        held.press(3);
        held.release(1);
        assert_eq!(held.drain(), vec![3, 2]);
        assert!(held.drain().is_empty());
    }
}
