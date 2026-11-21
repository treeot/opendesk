//! What extended input this host can inject, as told to the viewer.

use protocol::control::Control;

/// The `InputSupport` message for this host. Relative moves go through
/// enigo on every platform.
pub fn input_support() -> Control {
    Control::InputSupport { relative_mouse: true }
}
