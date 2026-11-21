//! Virtual gamepads for the viewer's controllers.
//!
//! Windows plugs in an Xbox 360 controller through the ViGEmBus driver,
//! which the host user must install (it is not part of Windows). Linux
//! creates an Xbox 360-like device through uinput, which needs write access
//! to /dev/uinput. macOS has no public API for a virtual gamepad, so macOS
//! hosts report it as unsupported.

use protocol::input::{GamepadState, MAX_GAMEPADS};

/// Whether this host can create virtual gamepads; `Err` holds one of the
/// `protocol::control::GAMEPAD_*` codes.
pub fn probe() -> Result<(), &'static str> {
    backend::probe()
}

/// The session's virtual controllers, one per viewer pad index. Each is
/// plugged in on the pad's first state and removed when the viewer reports
/// it disconnected, or when this is dropped at session end.
#[derive(Default)]
pub struct Gamepads {
    pads: [Option<backend::Pad>; MAX_GAMEPADS as usize],
    /// Set after a pad couldn't be created, so a missing driver is logged
    /// once rather than retried on every state.
    failed: bool,
}

impl Gamepads {
    pub fn apply(&mut self, state: &GamepadState) {
        let Some(slot) = self.pads.get_mut(usize::from(state.index)) else { return };
        if !state.connected {
            *slot = None;
            return;
        }
        if slot.is_none() {
            if self.failed {
                return;
            }
            match backend::Pad::create() {
                Ok(pad) => *slot = Some(pad),
                Err(error) => {
                    log::warn!("virtual gamepad unavailable: {error}");
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(pad) = slot {
            pad.update(state);
        }
    }
}

/// Standard-mapping button bits.
#[cfg_attr(not(any(target_os = "windows", target_os = "linux")), allow(dead_code))]
mod bits {
    pub const A: u32 = 1;
    pub const B: u32 = 1 << 1;
    pub const X: u32 = 1 << 2;
    pub const Y: u32 = 1 << 3;
    pub const LB: u32 = 1 << 4;
    pub const RB: u32 = 1 << 5;
    pub const BACK: u32 = 1 << 8;
    pub const START: u32 = 1 << 9;
    pub const LEFT_STICK: u32 = 1 << 10;
    pub const RIGHT_STICK: u32 = 1 << 11;
    pub const UP: u32 = 1 << 12;
    pub const DOWN: u32 = 1 << 13;
    pub const LEFT: u32 = 1 << 14;
    pub const RIGHT: u32 = 1 << 15;
    pub const GUIDE: u32 = 1 << 16;
}

/// The XInput report for `state`: buttons, triggers, then sticks with Y
/// pointing up as XInput expects.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn xinput_report(state: &GamepadState) -> (u16, [u8; 2], [i16; 4]) {
    const MAP: [(u32, u16); 15] = [
        (bits::UP, 0x0001),
        (bits::DOWN, 0x0002),
        (bits::LEFT, 0x0004),
        (bits::RIGHT, 0x0008),
        (bits::START, 0x0010),
        (bits::BACK, 0x0020),
        (bits::LEFT_STICK, 0x0040),
        (bits::RIGHT_STICK, 0x0080),
        (bits::LB, 0x0100),
        (bits::RB, 0x0200),
        (bits::GUIDE, 0x0400),
        (bits::A, 0x1000),
        (bits::B, 0x2000),
        (bits::X, 0x4000),
        (bits::Y, 0x8000),
    ];
    let buttons = MAP.iter().filter(|(bit, _)| state.buttons & bit != 0).fold(0, |all, (_, flag)| all | flag);
    let [lx, ly, rx, ry] = state.axes;
    (buttons, state.triggers, [lx, ly.saturating_neg(), rx, ry.saturating_neg()])
}

/// Linux input event codes for the uinput device, laid out like the kernel's
/// xpad driver so games and SDL treat it as an Xbox 360 pad.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod evdev {
    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_ABS: u16 = 0x03;
    pub const SYN_REPORT: u16 = 0;
    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;
    pub const ABS_Z: u16 = 0x02;
    pub const ABS_RX: u16 = 0x03;
    pub const ABS_RY: u16 = 0x04;
    pub const ABS_RZ: u16 = 0x05;
    pub const ABS_HAT0X: u16 = 0x10;
    pub const ABS_HAT0Y: u16 = 0x11;

    use super::bits;
    /// Standard-mapping bit to key code (BTN_A, BTN_B, BTN_X, BTN_Y, BTN_TL,
    /// BTN_TR, BTN_SELECT, BTN_START, BTN_MODE, BTN_THUMBL, BTN_THUMBR).
    pub const KEYS: [(u32, u16); 11] = [
        (bits::A, 0x130),
        (bits::B, 0x131),
        (bits::X, 0x133),
        (bits::Y, 0x134),
        (bits::LB, 0x136),
        (bits::RB, 0x137),
        (bits::BACK, 0x13a),
        (bits::START, 0x13b),
        (bits::GUIDE, 0x13c),
        (bits::LEFT_STICK, 0x13d),
        (bits::RIGHT_STICK, 0x13e),
    ];
    /// Axis code, minimum, maximum.
    pub const AXES: [(u16, i32, i32); 8] = [
        (ABS_X, -32_768, 32_767),
        (ABS_Y, -32_768, 32_767),
        (ABS_RX, -32_768, 32_767),
        (ABS_RY, -32_768, 32_767),
        (ABS_Z, 0, 255),
        (ABS_RZ, 0, 255),
        (ABS_HAT0X, -1, 1),
        (ABS_HAT0Y, -1, 1),
    ];
}

/// The (type, code, value) events that set a uinput pad to `state`, ending
/// with a sync. The kernel drops values that didn't change.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evdev_events(state: &GamepadState) -> Vec<(u16, u16, i32)> {
    use evdev::*;
    let held = |bit: u32| i32::from(state.buttons & bit != 0);
    let mut events: Vec<(u16, u16, i32)> = KEYS.iter().map(|&(bit, code)| (EV_KEY, code, held(bit))).collect();
    let [lx, ly, rx, ry] = state.axes.map(i32::from);
    events.extend([
        (EV_ABS, ABS_X, lx),
        (EV_ABS, ABS_Y, ly),
        (EV_ABS, ABS_RX, rx),
        (EV_ABS, ABS_RY, ry),
        (EV_ABS, ABS_Z, i32::from(state.triggers[0])),
        (EV_ABS, ABS_RZ, i32::from(state.triggers[1])),
        (EV_ABS, ABS_HAT0X, held(bits::RIGHT) - held(bits::LEFT)),
        (EV_ABS, ABS_HAT0Y, held(bits::DOWN) - held(bits::UP)),
        (EV_SYN, SYN_REPORT, 0),
    ]);
    events
}

#[cfg(target_os = "windows")]
mod backend {
    use protocol::control::{GAMEPAD_FAILED, GAMEPAD_VIGEMBUS_MISSING};
    use protocol::input::GamepadState;
    use vigem_client::{Client, Error, TargetId, XButtons, XGamepad, XTarget};

    fn code(error: Error) -> &'static str {
        if error == Error::BusNotFound { GAMEPAD_VIGEMBUS_MISSING } else { GAMEPAD_FAILED }
    }

    pub fn probe() -> Result<(), &'static str> {
        Client::connect().map(drop).map_err(code)
    }

    /// A plugged-in virtual Xbox 360 controller; dropping it unplugs it.
    pub struct Pad(XTarget);

    impl Pad {
        pub fn create() -> Result<Self, String> {
            let client = Client::connect().map_err(|error| format!("ViGEmBus: {error}"))?;
            let mut target = XTarget::new(client, TargetId::XBOX360_WIRED);
            target.plugin().map_err(|error| format!("plugging in the controller: {error}"))?;
            target.wait_ready().map_err(|error| format!("waiting for the controller: {error}"))?;
            Ok(Self(target))
        }

        pub fn update(&mut self, state: &GamepadState) {
            let (buttons, [left_trigger, right_trigger], [thumb_lx, thumb_ly, thumb_rx, thumb_ry]) = super::xinput_report(state);
            let report = XGamepad { buttons: XButtons(buttons), left_trigger, right_trigger, thumb_lx, thumb_ly, thumb_rx, thumb_ry };
            if let Err(error) = self.0.update(&report) {
                log::debug!("virtual gamepad update failed: {error}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    use protocol::control::{GAMEPAD_FAILED, GAMEPAD_NO_PERMISSION, GAMEPAD_UINPUT_MISSING};
    use protocol::input::GamepadState;

    use super::evdev;

    const UINPUT: &str = "/dev/uinput";

    /// `_IO`/`_IOW` request numbers for uinput ('U'). This is the layout on
    /// x86, ARM and RISC-V; MIPS, PowerPC and SPARC encode them differently.
    const fn ioc(write: bool, nr: u32, size: usize) -> u32 {
        ((write as u32) << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | nr
    }
    const UI_DEV_CREATE: u32 = ioc(false, 1, 0);
    const UI_DEV_DESTROY: u32 = ioc(false, 2, 0);
    const UI_DEV_SETUP: u32 = ioc(true, 3, size_of::<libc::uinput_setup>());
    const UI_ABS_SETUP: u32 = ioc(true, 4, size_of::<libc::uinput_abs_setup>());
    const UI_SET_EVBIT: u32 = ioc(true, 100, size_of::<libc::c_int>());
    const UI_SET_KEYBIT: u32 = ioc(true, 101, size_of::<libc::c_int>());
    const UI_SET_ABSBIT: u32 = ioc(true, 103, size_of::<libc::c_int>());

    fn open() -> std::io::Result<File> {
        OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(UINPUT)
    }

    pub fn probe() -> Result<(), &'static str> {
        match open() {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(GAMEPAD_UINPUT_MISSING),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => Err(GAMEPAD_NO_PERMISSION),
            Err(_) => Err(GAMEPAD_FAILED),
        }
    }

    /// A uinput device; dropping it destroys the device and closes the file.
    pub struct Pad(File);

    fn check(result: libc::c_int, what: &str) -> Result<(), String> {
        if result < 0 { Err(format!("{what}: {}", std::io::Error::last_os_error())) } else { Ok(()) }
    }

    impl Pad {
        pub fn create() -> Result<Self, String> {
            let file = open().map_err(|error| format!("opening {UINPUT}: {error}"))?;
            let fd = file.as_raw_fd();
            // SAFETY: uinput ioctls on our own open fd, each with the argument
            // type the kernel expects (an int, or a pointer to a filled struct).
            unsafe {
                check(libc::ioctl(fd, UI_SET_EVBIT as _, libc::c_int::from(evdev::EV_KEY)), "UI_SET_EVBIT")?;
                check(libc::ioctl(fd, UI_SET_EVBIT as _, libc::c_int::from(evdev::EV_ABS)), "UI_SET_EVBIT")?;
                for (_, key) in evdev::KEYS {
                    check(libc::ioctl(fd, UI_SET_KEYBIT as _, libc::c_int::from(key)), "UI_SET_KEYBIT")?;
                }
                for (axis, minimum, maximum) in evdev::AXES {
                    check(libc::ioctl(fd, UI_SET_ABSBIT as _, libc::c_int::from(axis)), "UI_SET_ABSBIT")?;
                    let mut abs: libc::uinput_abs_setup = std::mem::zeroed();
                    abs.code = axis;
                    abs.absinfo.minimum = minimum;
                    abs.absinfo.maximum = maximum;
                    if maximum > 255 {
                        abs.absinfo.fuzz = 16;
                        abs.absinfo.flat = 128;
                    }
                    check(libc::ioctl(fd, UI_ABS_SETUP as _, &raw const abs), "UI_ABS_SETUP")?;
                }
                let mut setup: libc::uinput_setup = std::mem::zeroed();
                // BUS_USB, and the Xbox 360 wired controller's ids.
                setup.id.bustype = 0x03;
                setup.id.vendor = 0x045e;
                setup.id.product = 0x028e;
                setup.id.version = 0x0110;
                for (slot, byte) in setup.name.iter_mut().zip(b"OpenDesk Virtual Gamepad") {
                    *slot = *byte as libc::c_char;
                }
                check(libc::ioctl(fd, UI_DEV_SETUP as _, &raw const setup), "UI_DEV_SETUP")?;
                check(libc::ioctl(fd, UI_DEV_CREATE as _), "UI_DEV_CREATE")?;
            }
            Ok(Self(file))
        }

        pub fn update(&mut self, state: &GamepadState) {
            let mut bytes = Vec::new();
            for (kind, code, value) in super::evdev_events(state) {
                // SAFETY: input_event is plain data; zeroed leaves the time
                // fields 0, which the kernel fills in.
                let mut event: libc::input_event = unsafe { std::mem::zeroed() };
                event.type_ = kind;
                event.code = code;
                event.value = value;
                // SAFETY: viewing a live, fully initialized struct as bytes.
                let raw = unsafe { std::slice::from_raw_parts((&event as *const libc::input_event).cast::<u8>(), size_of::<libc::input_event>()) };
                bytes.extend_from_slice(raw);
            }
            if let Err(error) = self.0.write_all(&bytes) {
                log::debug!("virtual gamepad update failed: {error}");
            }
        }
    }

    impl Drop for Pad {
        fn drop(&mut self) {
            // SAFETY: plain ioctl on our own fd, which closes right after.
            unsafe { libc::ioctl(self.0.as_raw_fd(), UI_DEV_DESTROY as _) };
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod backend {
    use protocol::control::GAMEPAD_UNSUPPORTED_OS;
    use protocol::input::GamepadState;

    pub fn probe() -> Result<(), &'static str> {
        Err(GAMEPAD_UNSUPPORTED_OS)
    }

    pub enum Pad {}

    impl Pad {
        pub fn create() -> Result<Self, String> {
            Err("virtual gamepads are not supported on this OS".into())
        }

        pub fn update(&mut self, _state: &GamepadState) {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(buttons: u32, axes: [i16; 4], triggers: [u8; 2]) -> GamepadState {
        GamepadState { index: 0, connected: true, buttons, axes, triggers }
    }

    #[test]
    fn xinput_flips_the_y_axes_and_maps_buttons() {
        let (buttons, triggers, axes) = xinput_report(&state(bits::A | bits::UP | bits::GUIDE | 1 << 6, [100, 32_767, -5, -32_768], [10, 255]));
        // The LT bit (6) is analog only in XInput.
        assert_eq!(buttons, 0x1000 | 0x0001 | 0x0400);
        assert_eq!(triggers, [10, 255]);
        assert_eq!(axes, [100, -32_767, -5, 32_767]);
    }

    #[test]
    fn evdev_events_cover_keys_axes_and_the_dpad_hat() {
        let events = evdev_events(&state(bits::B | bits::LEFT | bits::DOWN, [1, 2, 3, 4], [5, 6]));
        assert_eq!(events.last(), Some(&(evdev::EV_SYN, evdev::SYN_REPORT, 0)));
        assert!(events.contains(&(evdev::EV_KEY, 0x131, 1)));
        assert!(events.contains(&(evdev::EV_KEY, 0x130, 0)));
        assert!(events.contains(&(evdev::EV_ABS, evdev::ABS_Y, 2)));
        assert!(events.contains(&(evdev::EV_ABS, evdev::ABS_RZ, 6)));
        assert!(events.contains(&(evdev::EV_ABS, evdev::ABS_HAT0X, -1)));
        assert!(events.contains(&(evdev::EV_ABS, evdev::ABS_HAT0Y, 1)));
    }

    #[test]
    fn unsupported_hosts_ignore_pads() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(probe(), Err(protocol::control::GAMEPAD_UNSUPPORTED_OS));
            let mut pads = Gamepads::default();
            pads.apply(&state(bits::A, [0; 4], [0; 2]));
            assert!(pads.failed);
            pads.apply(&GamepadState { connected: false, ..state(0, [0; 4], [0; 2]) });
        }
    }
}
