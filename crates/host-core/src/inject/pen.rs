//! Pen and touch input from the viewer.
//!
//! Windows injects real pen and touch contacts (pressure, tilt, several
//! fingers) through the synthetic pointer API. Elsewhere, or if Windows
//! refuses to create the device, the primary contact drives the mouse: down
//! presses the left button (the right one with the pen's barrel button
//! held), moves drag, up releases. macOS and Linux have no public API for
//! injecting pen or touch input, so they always emulate.

use protocol::input::{InputEvent, PointerContact, PointerKind, PointerPhase, PEN_BARREL};

use super::DisplayBounds;

/// Pen and touch state for one session's injector.
#[derive(Default)]
pub struct PenInjector {
    emulator: Emulator,
    #[cfg(target_os = "windows")]
    native: native::Native,
}

impl PenInjector {
    /// Inject `contact` natively where possible. Returns the mouse events
    /// to inject instead when it can't be (empty once handled natively).
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    pub fn inject(&mut self, contact: &PointerContact, bounds: Option<DisplayBounds>) -> Vec<InputEvent> {
        #[cfg(target_os = "windows")]
        if let Some(bounds) = bounds {
            let (x, y) = super::map_to_bounds(contact.x, contact.y, bounds);
            if self.native.inject(contact, x, y) {
                return Vec::new();
            }
        }
        self.emulator.handle(contact)
    }
}

/// Whether this host injects real pen and touch input: `(pen, touch)`.
pub fn native_support() -> (bool, bool) {
    #[cfg(target_os = "windows")]
    {
        native::available()
    }
    #[cfg(not(target_os = "windows"))]
    {
        (false, false)
    }
}

/// Mouse emulation: the first contact to go down owns the pointer until it
/// lifts; other fingers are ignored. A hovering pen moves the pointer.
#[derive(Default)]
struct Emulator {
    /// The owning contact and the button it pressed.
    active: Option<(PointerKind, u32, u8)>,
}

const LEFT: u8 = 1;
const RIGHT: u8 = 2;

impl Emulator {
    fn handle(&mut self, contact: &PointerContact) -> Vec<InputEvent> {
        let position = InputEvent::MouseMove { x: contact.x, y: contact.y };
        let owner = self.active.filter(|(kind, id, _)| *kind == contact.kind && *id == contact.id);
        match contact.phase {
            PointerPhase::Down if self.active.is_none() => {
                let button = if contact.buttons & PEN_BARREL != 0 { RIGHT } else { LEFT };
                self.active = Some((contact.kind, contact.id, button));
                vec![position, InputEvent::MouseButton { button, down: true }]
            }
            PointerPhase::Hover | PointerPhase::Move if owner.is_some() => vec![position],
            PointerPhase::Hover if self.active.is_none() && contact.kind == PointerKind::Pen => vec![position],
            PointerPhase::Up | PointerPhase::Cancel | PointerPhase::Leave => match owner {
                Some((_, _, button)) => {
                    self.active = None;
                    let release = InputEvent::MouseButton { button, down: false };
                    if contact.phase == PointerPhase::Up { vec![position, release] } else { vec![release] }
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

#[cfg(target_os = "windows")]
mod native {
    use protocol::input::{PointerContact, PointerKind, PointerPhase, PEN_BARREL, PEN_ERASER};
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::UI::Controls::{
        CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
        POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
    };
    use windows::Win32::UI::Input::Pointer::{
        InjectSyntheticPointerInput, POINTER_BUTTON_CHANGE_TYPE, POINTER_CHANGE_FIRSTBUTTON_DOWN,
        POINTER_CHANGE_FIRSTBUTTON_UP, POINTER_CHANGE_NONE, POINTER_FLAGS, POINTER_FLAG_CANCELED,
        POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON, POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE,
        POINTER_FLAG_UP, POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_NONE, PEN_MASK_PRESSURE, PEN_MASK_TILT_X,
        PEN_MASK_TILT_Y, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, TOUCH_MASK_CONTACTAREA,
        TOUCH_MASK_PRESSURE,
    };

    /// Most simultaneous touch contacts injected.
    const MAX_TOUCHES: usize = 10;
    /// Half the side of the reported touch contact area, in pixels.
    const TOUCH_RADIUS: i32 = 2;

    struct Device(HSYNTHETICPOINTERDEVICE);

    impl Drop for Device {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateSyntheticPointerDevice and is
            // destroyed exactly once.
            unsafe { DestroySyntheticPointerDevice(self.0) };
        }
    }

    fn create(kind: POINTER_INPUT_TYPE, max: u32) -> Option<Device> {
        // SAFETY: plain Win32 call; failure (pre-1809 Windows, no desktop
        // access) returns an error.
        unsafe { CreateSyntheticPointerDevice(kind, max, POINTER_FEEDBACK_DEFAULT) }.ok().map(Device)
    }

    /// Probe by creating and dropping each device.
    pub fn available() -> (bool, bool) {
        (create(PT_PEN, 1).is_some(), create(PT_TOUCH, MAX_TOUCHES as u32).is_some())
    }

    /// A device made on first use; `Failed` sticks so a refusing system
    /// falls back to mouse emulation without retrying every event.
    #[derive(Default)]
    enum Slot {
        #[default]
        Untried,
        Ready(Device),
        Failed,
    }

    impl Slot {
        fn get(&mut self, kind: POINTER_INPUT_TYPE, max: u32) -> Option<&Device> {
            if matches!(self, Slot::Untried) {
                *self = match create(kind, max) {
                    Some(device) => Slot::Ready(device),
                    None => {
                        log::warn!("synthetic pointer device unavailable; emulating the mouse instead");
                        Slot::Failed
                    }
                };
            }
            match self {
                Slot::Ready(device) => Some(device),
                _ => None,
            }
        }
    }

    #[derive(Clone, Copy)]
    struct Touch {
        viewer_id: u32,
        point: POINT,
        pressure: u32,
    }

    #[derive(Default)]
    pub struct Native {
        pen: Slot,
        touch: Slot,
        touches: [Option<Touch>; MAX_TOUCHES],
    }

    /// 0..=65535 to the 0..=1024 Windows uses.
    fn pressure(value: u16) -> u32 {
        u32::from(value) * 1024 / 65_535
    }

    fn pointer_info(kind: POINTER_INPUT_TYPE, id: u32, point: POINT, flags: POINTER_FLAGS, change: POINTER_BUTTON_CHANGE_TYPE) -> POINTER_INFO {
        POINTER_INFO {
            pointerType: kind,
            pointerId: id,
            pointerFlags: flags,
            ptPixelLocation: point,
            ButtonChangeType: change,
            ..Default::default()
        }
    }

    impl Native {
        /// `(x, y)` is the contact in virtual-screen pixels. False when the
        /// device can't be created, so the caller emulates instead.
        pub fn inject(&mut self, contact: &PointerContact, x: i32, y: i32) -> bool {
            let point = POINT { x, y };
            match contact.kind {
                PointerKind::Pen => self.pen(contact, point),
                PointerKind::Touch => self.touch(contact, point),
            }
        }

        fn pen(&mut self, contact: &PointerContact, point: POINT) -> bool {
            let Some(device) = self.pen.get(PT_PEN, 1) else { return false };
            let (flags, change) = match contact.phase {
                PointerPhase::Hover => (POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE, POINTER_CHANGE_NONE),
                PointerPhase::Down => (POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON | POINTER_FLAG_DOWN, POINTER_CHANGE_FIRSTBUTTON_DOWN),
                PointerPhase::Move => (POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON | POINTER_FLAG_UPDATE, POINTER_CHANGE_NONE),
                PointerPhase::Up => (POINTER_FLAG_INRANGE | POINTER_FLAG_UP, POINTER_CHANGE_FIRSTBUTTON_UP),
                PointerPhase::Cancel => (POINTER_FLAG_UP | POINTER_FLAG_CANCELED, POINTER_CHANGE_FIRSTBUTTON_UP),
                PointerPhase::Leave => (POINTER_FLAG_UPDATE, POINTER_CHANGE_NONE),
            };
            let mut pen_flags = PEN_FLAG_NONE;
            if contact.buttons & PEN_BARREL != 0 {
                pen_flags |= PEN_FLAG_BARREL;
            }
            if contact.buttons & PEN_ERASER != 0 {
                pen_flags |= PEN_FLAG_ERASER;
            }
            let info = POINTER_TYPE_INFO {
                r#type: PT_PEN,
                Anonymous: POINTER_TYPE_INFO_0 {
                    penInfo: POINTER_PEN_INFO {
                        pointerInfo: pointer_info(PT_PEN, 0, point, flags, change),
                        penFlags: pen_flags,
                        penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                        pressure: pressure(contact.pressure),
                        rotation: 0,
                        tiltX: i32::from(contact.tilt_x).clamp(-90, 90),
                        tiltY: i32::from(contact.tilt_y).clamp(-90, 90),
                    },
                },
            };
            // SAFETY: one fully initialized POINTER_TYPE_INFO for this device.
            if let Err(error) = unsafe { InjectSyntheticPointerInput(device.0, &[info]) } {
                log::debug!("pen injection failed: {error}");
            }
            true
        }

        /// Windows wants every contact that is down in each frame, so the
        /// changed one goes in with all the others still held.
        fn touch(&mut self, contact: &PointerContact, point: POINT) -> bool {
            if self.touch.get(PT_TOUCH, MAX_TOUCHES as u32).is_none() {
                return false;
            }
            let existing = self.touches.iter().position(|t| t.is_some_and(|t| t.viewer_id == contact.id));
            let slot = match (existing, contact.phase) {
                (Some(slot), _) => slot,
                (None, PointerPhase::Down) => match self.touches.iter().position(Option::is_none) {
                    Some(slot) => slot,
                    None => return true, // more fingers than we inject
                },
                _ => return true,
            };
            self.touches[slot] = Some(Touch { viewer_id: contact.id, point, pressure: pressure(contact.pressure) });
            let changed = match contact.phase {
                PointerPhase::Down => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                PointerPhase::Hover | PointerPhase::Move => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
                PointerPhase::Up | PointerPhase::Leave => POINTER_FLAG_UP,
                PointerPhase::Cancel => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
            };
            let frame: Vec<POINTER_TYPE_INFO> = self
                .touches
                .iter()
                .enumerate()
                .filter_map(|(index, touch)| touch.map(|touch| (index, touch)))
                .map(|(index, touch)| {
                    let flags = if index == slot { changed } else { POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT };
                    let p = touch.point;
                    POINTER_TYPE_INFO {
                        r#type: PT_TOUCH,
                        Anonymous: POINTER_TYPE_INFO_0 {
                            touchInfo: POINTER_TOUCH_INFO {
                                pointerInfo: pointer_info(PT_TOUCH, index as u32, p, flags, POINTER_CHANGE_NONE),
                                touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_PRESSURE,
                                rcContact: RECT { left: p.x - TOUCH_RADIUS, top: p.y - TOUCH_RADIUS, right: p.x + TOUCH_RADIUS, bottom: p.y + TOUCH_RADIUS },
                                pressure: touch.pressure,
                                ..Default::default()
                            },
                        },
                    }
                })
                .collect();
            if matches!(contact.phase, PointerPhase::Up | PointerPhase::Cancel | PointerPhase::Leave) {
                self.touches[slot] = None;
            }
            if let Some(device) = self.touch.get(PT_TOUCH, MAX_TOUCHES as u32) {
                // SAFETY: fully initialized POINTER_TYPE_INFO values for this device.
                if let Err(error) = unsafe { InjectSyntheticPointerInput(device.0, &frame) } {
                    log::debug!("touch injection failed: {error}");
                }
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(kind: PointerKind, phase: PointerPhase, id: u32, x: i32) -> PointerContact {
        PointerContact { kind, phase, id, x, y: x, pressure: 30_000, tilt_x: 0, tilt_y: 0, buttons: 0 }
    }

    #[test]
    fn primary_touch_drags_the_left_button_and_others_are_ignored() {
        let mut emulator = Emulator::default();
        let touch = |phase, id, x| contact(PointerKind::Touch, phase, id, x);
        let mv = |x| InputEvent::MouseMove { x, y: x };
        let button = |down| InputEvent::MouseButton { button: LEFT, down };
        assert_eq!(emulator.handle(&touch(PointerPhase::Down, 4, 10)), vec![mv(10), button(true)]);
        // A second finger neither moves nor clicks.
        assert!(emulator.handle(&touch(PointerPhase::Down, 5, 90)).is_empty());
        assert!(emulator.handle(&touch(PointerPhase::Move, 5, 91)).is_empty());
        assert_eq!(emulator.handle(&touch(PointerPhase::Move, 4, 20)), vec![mv(20)]);
        assert!(emulator.handle(&touch(PointerPhase::Up, 5, 91)).is_empty());
        assert_eq!(emulator.handle(&touch(PointerPhase::Up, 4, 30)), vec![mv(30), button(false)]);
        // Free again for the next contact.
        assert_eq!(emulator.handle(&touch(PointerPhase::Down, 5, 40)), vec![mv(40), button(true)]);
        assert_eq!(emulator.handle(&touch(PointerPhase::Cancel, 5, 40)), vec![button(false)]);
    }

    #[test]
    fn pen_hovers_and_its_barrel_button_right_clicks() {
        let mut emulator = Emulator::default();
        let pen = |phase, x| contact(PointerKind::Pen, phase, 1, x);
        assert_eq!(emulator.handle(&pen(PointerPhase::Hover, 5)), vec![InputEvent::MouseMove { x: 5, y: 5 }]);
        let mut down = pen(PointerPhase::Down, 6);
        down.buttons = PEN_BARREL;
        assert_eq!(emulator.handle(&down), vec![InputEvent::MouseMove { x: 6, y: 6 }, InputEvent::MouseButton { button: RIGHT, down: true }]);
        // Leaving range mid-stroke still lets the button go.
        assert_eq!(emulator.handle(&pen(PointerPhase::Leave, 6)), vec![InputEvent::MouseButton { button: RIGHT, down: false }]);
        assert!(emulator.handle(&pen(PointerPhase::Leave, 6)).is_empty());
        // While a touch owns the pointer, pen hover is ignored.
        emulator.handle(&contact(PointerKind::Touch, PointerPhase::Down, 9, 1));
        assert!(emulator.handle(&pen(PointerPhase::Hover, 7)).is_empty());
    }
}
