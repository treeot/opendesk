/// Wire tags of the extended input events. They start at 16, clear of the
/// original 1..=5, so other additions can take the low numbers.
const TAG_RELATIVE_MOUSE_MOVE: u8 = 16;
const TAG_POINTER: u8 = 17;
const TAG_GAMEPAD: u8 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Key {
        code: u32,
        down: bool,
        modifiers: u8,
    },
    Char {
        ch: char,
        down: bool,
        modifiers: u8,
    },
    /// Pointer-locked mouse motion in device units, for games and 3D apps
    /// that read raw deltas. Only sent to hosts that report `relative_mouse`
    /// in `Control::InputSupport`; older hosts drop the unknown tag.
    RelativeMouseMove {
        dx: i32,
        dy: i32,
    },
    /// A pen or touch contact. Only sent to hosts that report `pen_touch` in
    /// `Control::InputSupport`.
    Pointer(PointerContact),
    /// The full state of one of the viewer's gamepads. Only sent to hosts
    /// that report `gamepad` in `Control::InputSupport`.
    Gamepad(GamepadState),
    /// A physical key, by position rather than by what it types: `usage` is
    /// the USB HID usage (page in the high 16 bits, so `0x0007_0004` is the
    /// key labelled A on a US keyboard). The host presses the key in the same
    /// place on its own keyboard, and its layout decides the character.
    /// `modifiers` are held around the press like `Key`'s; map mode sends 0
    /// and forwards the modifier keys themselves. Only sent to hosts that
    /// announced `Control::KeyboardModeSupport`: older hosts drop the tag.
    ScanKey {
        usage: u32,
        down: bool,
        modifiers: u8,
    },
    /// Type `ch` as text, whatever the host's keyboard layout. A single
    /// action, so there is no release. Same capability rule as `ScanKey`.
    TypeChar {
        ch: char,
    },
}

/// Wire tags of the keyboard-layout events. Kept well clear of the low tags
/// so other new variants can take the next small numbers.
const SCAN_KEY_TAG: u8 = 32;
const TYPE_CHAR_TAG: u8 = 33;

impl InputEvent {
    pub fn to_bytes(&self, timestamp_us: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(match self {
            Self::MouseMove { .. } => 1,
            Self::MouseButton { .. } => 2,
            Self::Scroll { .. } => 3,
            Self::Key { .. } => 4,
            Self::Char { .. } => 5,
            Self::RelativeMouseMove { .. } => TAG_RELATIVE_MOUSE_MOVE,
            Self::Pointer(_) => TAG_POINTER,
            Self::Gamepad(_) => TAG_GAMEPAD,
            Self::ScanKey { .. } => SCAN_KEY_TAG,
            Self::TypeChar { .. } => TYPE_CHAR_TAG,
        });
        bytes.extend_from_slice(&timestamp_us.to_be_bytes());

        match self {
            Self::MouseMove { x, y } => {
                bytes.extend_from_slice(&x.to_be_bytes());
                bytes.extend_from_slice(&y.to_be_bytes());
            }
            Self::MouseButton { button, down } => {
                bytes.push(*button);
                bytes.push(u8::from(*down));
            }
            Self::Scroll { dx, dy } => {
                bytes.extend_from_slice(&dx.to_be_bytes());
                bytes.extend_from_slice(&dy.to_be_bytes());
            }
            Self::Key {
                code,
                down,
                modifiers,
            } => {
                bytes.extend_from_slice(&code.to_be_bytes());
                bytes.push(u8::from(*down));
                bytes.push(*modifiers);
            }
            Self::Char {
                ch,
                down,
                modifiers,
            } => {
                bytes.extend_from_slice(&(*ch as u32).to_be_bytes());
                bytes.push(u8::from(*down));
                bytes.push(*modifiers);
            }
            Self::RelativeMouseMove { dx, dy } => {
                bytes.extend_from_slice(&dx.to_be_bytes());
                bytes.extend_from_slice(&dy.to_be_bytes());
            }
            Self::Pointer(contact) => contact.write(&mut bytes),
            Self::Gamepad(state) => state.write(&mut bytes),
            Self::ScanKey {
                usage,
                down,
                modifiers,
            } => {
                bytes.extend_from_slice(&usage.to_be_bytes());
                bytes.push(u8::from(*down));
                bytes.push(*modifiers);
            }
            Self::TypeChar { ch } => {
                bytes.extend_from_slice(&(*ch as u32).to_be_bytes());
            }
        }

        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<(InputEvent, u64)> {
        let kind = *bytes.first()?;
        let timestamp_us = u64::from_be_bytes(bytes.get(1..9)?.try_into().ok()?);

        match kind {
            1 if bytes.len() == 17 => {
                let x = i32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?);
                let y = i32::from_be_bytes(bytes.get(13..17)?.try_into().ok()?);
                Some((Self::MouseMove { x, y }, timestamp_us))
            }
            2 if bytes.len() == 11 => {
                let button = bytes[9];
                let down = match bytes[10] {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Some((Self::MouseButton { button, down }, timestamp_us))
            }
            3 if bytes.len() == 17 => {
                let dx = i32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?);
                let dy = i32::from_be_bytes(bytes.get(13..17)?.try_into().ok()?);
                Some((Self::Scroll { dx, dy }, timestamp_us))
            }
            4 if bytes.len() == 15 => {
                let code = u32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?);
                let down = match bytes[13] {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Some((
                    Self::Key {
                        code,
                        down,
                        modifiers: bytes[14],
                    },
                    timestamp_us,
                ))
            }
            5 if bytes.len() == 15 => {
                let ch = char::from_u32(u32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?))
                    .filter(|ch| !ch.is_control())?;
                let down = match bytes[13] {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Some((
                    Self::Char {
                        ch,
                        down,
                        modifiers: bytes[14],
                    },
                    timestamp_us,
                ))
            }
            TAG_RELATIVE_MOUSE_MOVE if bytes.len() == 17 => {
                let dx = i32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?);
                let dy = i32::from_be_bytes(bytes.get(13..17)?.try_into().ok()?);
                Some((Self::RelativeMouseMove { dx, dy }, timestamp_us))
            }
            TAG_POINTER if bytes.len() == 9 + PointerContact::LEN => {
                Some((Self::Pointer(PointerContact::read(&bytes[9..])?), timestamp_us))
            }
            TAG_GAMEPAD if bytes.len() == 9 + GamepadState::LEN => {
                Some((Self::Gamepad(GamepadState::read(&bytes[9..])?), timestamp_us))
            }
            SCAN_KEY_TAG if bytes.len() == 15 => {
                let usage = u32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?);
                let down = match bytes[13] {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                Some((
                    Self::ScanKey {
                        usage,
                        down,
                        modifiers: bytes[14],
                    },
                    timestamp_us,
                ))
            }
            TYPE_CHAR_TAG if bytes.len() == 13 => {
                let ch = char::from_u32(u32::from_be_bytes(bytes.get(9..13)?.try_into().ok()?))
                    .filter(|ch| !ch.is_control())?;
                Some((Self::TypeChar { ch }, timestamp_us))
            }
            _ => None,
        }
    }
}

/// What kind of device a [`PointerContact`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    Pen = 1,
    Touch = 2,
}

/// Where a contact is in its life, as in DOM pointer events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    /// A pen moving within range of the surface without touching it.
    Hover = 0,
    Down = 1,
    /// Moving while touching.
    Move = 2,
    Up = 3,
    /// The contact ended without a deliberate lift (focus loss, palm
    /// rejection); the host should not treat it as a tap.
    Cancel = 4,
    /// A hovering pen went out of range.
    Leave = 5,
}

/// Pen barrel (side) button held.
pub const PEN_BARREL: u8 = 1;
/// The pen's eraser end is in use.
pub const PEN_ERASER: u8 = 1 << 1;

/// One pen or touch contact sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerContact {
    pub kind: PointerKind,
    pub phase: PointerPhase,
    /// Stable for the life of the contact (the viewer's pointerId).
    pub id: u32,
    /// Position in the same 0..=65535 space as `MouseMove`.
    pub x: i32,
    pub y: i32,
    /// 0..=65535 maps to no pressure..full pressure.
    pub pressure: u16,
    /// Degrees, -90..=90, as DOM tiltX/tiltY.
    pub tilt_x: i8,
    pub tilt_y: i8,
    /// [`PEN_BARREL`] and [`PEN_ERASER`] bits.
    pub buttons: u8,
}

impl PointerContact {
    /// Encoded size after the event header.
    const LEN: usize = 19;

    fn write(&self, bytes: &mut Vec<u8>) {
        bytes.push(self.kind as u8);
        bytes.push(self.phase as u8);
        bytes.extend_from_slice(&self.id.to_be_bytes());
        bytes.extend_from_slice(&self.x.to_be_bytes());
        bytes.extend_from_slice(&self.y.to_be_bytes());
        bytes.extend_from_slice(&self.pressure.to_be_bytes());
        bytes.extend_from_slice(&self.tilt_x.to_be_bytes());
        bytes.extend_from_slice(&self.tilt_y.to_be_bytes());
        bytes.push(self.buttons);
    }

    fn read(bytes: &[u8]) -> Option<Self> {
        let kind = match bytes.first()? {
            1 => PointerKind::Pen,
            2 => PointerKind::Touch,
            _ => return None,
        };
        let phase = match bytes.get(1)? {
            0 => PointerPhase::Hover,
            1 => PointerPhase::Down,
            2 => PointerPhase::Move,
            3 => PointerPhase::Up,
            4 => PointerPhase::Cancel,
            5 => PointerPhase::Leave,
            _ => return None,
        };
        Some(Self {
            kind,
            phase,
            id: u32::from_be_bytes(bytes.get(2..6)?.try_into().ok()?),
            x: i32::from_be_bytes(bytes.get(6..10)?.try_into().ok()?),
            y: i32::from_be_bytes(bytes.get(10..14)?.try_into().ok()?),
            pressure: u16::from_be_bytes(bytes.get(14..16)?.try_into().ok()?),
            tilt_x: i8::from_be_bytes([*bytes.get(16)?]),
            tilt_y: i8::from_be_bytes([*bytes.get(17)?]),
            buttons: *bytes.get(18)?,
        })
    }
}

/// Most gamepads one viewer forwards (XInput's limit too).
pub const MAX_GAMEPADS: u8 = 4;

/// One gamepad, laid out like the W3C "standard" gamepad mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamepadState {
    /// Which of the viewer's pads, below [`MAX_GAMEPADS`].
    pub index: u8,
    /// False once the pad is unplugged or forwarding stops; the host then
    /// removes its virtual controller.
    pub connected: bool,
    /// Bit `i` is standard-mapping button `i`: 0 A, 1 B, 2 X, 3 Y, 4 LB,
    /// 5 RB, 6 LT, 7 RT, 8 Back, 9 Start, 10 left stick, 11 right stick,
    /// 12 up, 13 down, 14 left, 15 right, 16 Guide.
    pub buttons: u32,
    /// Left X, left Y, right X, right Y; positive is right and down.
    pub axes: [i16; 4],
    /// Left and right trigger, 0 released to 255 fully pressed.
    pub triggers: [u8; 2],
}

impl GamepadState {
    /// Encoded size after the event header.
    const LEN: usize = 16;

    fn write(&self, bytes: &mut Vec<u8>) {
        bytes.push(self.index);
        bytes.push(u8::from(self.connected));
        bytes.extend_from_slice(&self.buttons.to_be_bytes());
        for axis in self.axes {
            bytes.extend_from_slice(&axis.to_be_bytes());
        }
        bytes.extend_from_slice(&self.triggers);
    }

    fn read(bytes: &[u8]) -> Option<Self> {
        let index = *bytes.first()?;
        if index >= MAX_GAMEPADS {
            return None;
        }
        let connected = match bytes.get(1)? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let axis = |at: usize| Some(i16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
        Some(Self {
            index,
            connected,
            buttons: u32::from_be_bytes(bytes.get(2..6)?.try_into().ok()?),
            axes: [axis(6)?, axis(8)?, axis(10)?, axis(12)?],
            triggers: [*bytes.get(14)?, *bytes.get(15)?],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_move_roundtrips() {
        let e = InputEvent::MouseMove { x: 30000, y: 45000 };
        let bytes = e.to_bytes(123);
        assert_eq!(InputEvent::from_bytes(&bytes), Some((e, 123)));
    }

    #[test]
    fn key_roundtrips() {
        let e = InputEvent::Key {
            code: '!' as u32,
            down: true,
            modifiers: 0b1010,
        };
        let bytes = e.to_bytes(9);
        assert_eq!(InputEvent::from_bytes(&bytes), Some((e, 9)));
    }

    #[test]
    fn printable_char_roundtrips_with_modifiers() {
        let e = InputEvent::Char {
            ch: '!',
            down: true,
            modifiers: 0b1010,
        };
        let bytes = e.to_bytes(10);
        assert_eq!(InputEvent::from_bytes(&bytes), Some((e, 10)));
    }

    #[test]
    fn button_and_scroll_roundtrip() {
        for e in [
            InputEvent::MouseButton {
                button: 1,
                down: false,
            },
            InputEvent::Scroll { dx: -3, dy: 7 },
        ] {
            let b = e.to_bytes(1);
            assert_eq!(InputEvent::from_bytes(&b), Some((e, 1)));
        }
    }

    #[test]
    fn relative_mouse_move_roundtrips() {
        let e = InputEvent::RelativeMouseMove { dx: -12, dy: 7 };
        let bytes = e.to_bytes(11);
        assert_eq!(bytes[0], 16);
        assert_eq!(bytes.len(), 17);
        assert_eq!(InputEvent::from_bytes(&bytes), Some((e, 11)));
        assert!(InputEvent::from_bytes(&bytes[..16]).is_none());
    }

    #[test]
    fn pointer_contact_roundtrips_and_rejects_unknown_kinds() {
        let e = InputEvent::Pointer(PointerContact {
            kind: PointerKind::Pen,
            phase: PointerPhase::Move,
            id: 7,
            x: 100,
            y: 65_535,
            pressure: 40_000,
            tilt_x: -45,
            tilt_y: 30,
            buttons: PEN_BARREL,
        });
        let bytes = e.to_bytes(5);
        assert_eq!((bytes[0], bytes.len()), (17, 28));
        assert_eq!(InputEvent::from_bytes(&bytes), Some((e, 5)));
        let mut bad = bytes.clone();
        bad[9] = 3;
        assert!(InputEvent::from_bytes(&bad).is_none());
        bad[9] = 2;
        bad[10] = 6;
        assert!(InputEvent::from_bytes(&bad).is_none());
        assert!(InputEvent::from_bytes(&bytes[..27]).is_none());
    }

    #[test]
    fn gamepad_state_roundtrips_and_rejects_bad_indexes() {
        let e = InputEvent::Gamepad(GamepadState {
            index: 1,
            connected: true,
            buttons: 1 | 1 << 16,
            axes: [-32_768, 32_767, 0, -1],
            triggers: [0, 255],
        });
        let bytes = e.to_bytes(3);
        assert_eq!((bytes[0], bytes.len()), (18, 25));
        assert_eq!(InputEvent::from_bytes(&bytes), Some((e, 3)));
        let mut bad = bytes.clone();
        bad[9] = MAX_GAMEPADS;
        assert!(InputEvent::from_bytes(&bad).is_none());
        bad[9] = 0;
        bad[10] = 2;
        assert!(InputEvent::from_bytes(&bad).is_none());
    }

    #[test]
    fn keyboard_layout_events_roundtrip() {
        for e in [
            InputEvent::ScanKey {
                usage: 0x0007_0004,
                down: true,
                modifiers: 0,
            },
            InputEvent::ScanKey {
                usage: 0x0007_0052,
                down: false,
                modifiers: 0b0011,
            },
            InputEvent::TypeChar { ch: '@' },
            InputEvent::TypeChar { ch: '\u{1F600}' },
        ] {
            let b = e.to_bytes(5);
            assert_eq!(InputEvent::from_bytes(&b), Some((e, 5)));
        }
        let scan = InputEvent::ScanKey {
            usage: 0x0007_0004,
            down: true,
            modifiers: 1,
        }
        .to_bytes(0);
        assert_eq!((scan[0], scan.len()), (32, 15));
        let typed = InputEvent::TypeChar { ch: 'a' }.to_bytes(0);
        assert_eq!((typed[0], typed.len()), (33, 13));
    }

    #[test]
    fn type_char_rejects_controls_and_bad_lengths() {
        let mut b = InputEvent::TypeChar { ch: 'a' }.to_bytes(0);
        b[12] = b'\n';
        assert!(InputEvent::from_bytes(&b).is_none());
        let mut long = InputEvent::TypeChar { ch: 'a' }.to_bytes(0);
        long.push(0);
        assert!(InputEvent::from_bytes(&long).is_none());
        let mut scan = InputEvent::ScanKey {
            usage: 4,
            down: true,
            modifiers: 0,
        }
        .to_bytes(0);
        scan[13] = 2;
        assert!(InputEvent::from_bytes(&scan).is_none());
    }

    #[test]
    fn rejects_garbage() {
        assert!(InputEvent::from_bytes(&[]).is_none());
        assert!(InputEvent::from_bytes(&[0xFF, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
