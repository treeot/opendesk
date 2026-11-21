//! What extended input this host can inject, as told to the viewer.

use protocol::control::Control;

/// The `InputSupport` message for this host. Relative moves go through
/// enigo on every platform. Pen and touch are always accepted; only Windows
/// injects them as real pen and touch, other hosts move the mouse. Gamepads
/// need ViGEmBus on Windows and a writable /dev/uinput on Linux, and are
/// never available on macOS.
pub fn input_support() -> Control {
    let (native_pen, native_touch) = super::pen::native_support();
    let gamepad = super::gamepad::probe();
    Control::InputSupport {
        relative_mouse: true,
        pen_touch: true,
        native_pen,
        native_touch,
        gamepad: gamepad.is_ok(),
        gamepad_unavailable: gamepad.err().map(str::to_owned),
    }
}
