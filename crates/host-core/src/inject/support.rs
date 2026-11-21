//! What extended input this host can inject, as told to the viewer.

use protocol::control::Control;

/// The `InputSupport` message for this host. Relative moves go through
/// enigo on every platform. Pen and touch are always accepted; only Windows
/// injects them as real pen and touch, other hosts move the mouse.
pub fn input_support() -> Control {
    let (native_pen, native_touch) = super::pen::native_support();
    Control::InputSupport { relative_mouse: true, pen_touch: true, native_pen, native_touch }
}
