/// Wire tags of the extended input events. They start at 16, clear of the
/// original 1..=5, so other additions can take the low numbers.
const TAG_RELATIVE_MOUSE_MOVE: u8 = 16;

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
}

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
            _ => None,
        }
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
    fn rejects_garbage() {
        assert!(InputEvent::from_bytes(&[]).is_none());
        assert!(InputEvent::from_bytes(&[0xFF, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
