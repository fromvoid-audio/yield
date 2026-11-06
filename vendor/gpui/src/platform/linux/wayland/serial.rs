use collections::HashMap;

#[derive(Debug, Hash, PartialEq, Eq)]
pub(crate) enum SerialKind {
    DataDevice,
    InputMethod,
    MouseEnter,
    MousePress,
    KeyPress,
    #[cfg(any(target_os = "linux", test))]
    ClipboardInput,
}

#[derive(Debug)]
struct SerialData {
    serial: u32,
}

impl SerialData {
    fn new(value: u32) -> Self {
        Self { serial: value }
    }
}

#[derive(Debug)]
/// Helper for tracking of different serial kinds.
pub(crate) struct SerialTracker {
    serials: HashMap<SerialKind, SerialData>,
}

impl SerialTracker {
    pub fn new() -> Self {
        Self {
            serials: HashMap::default(),
        }
    }

    pub fn update(&mut self, kind: SerialKind, value: u32) {
        self.serials.insert(kind, SerialData::new(value));
    }

    /// Returns the latest tracked serial of the provided [`SerialKind`]
    ///
    /// Will return 0 if not tracked.
    pub fn get(&self, kind: SerialKind) -> u32 {
        self.serials
            .get(&kind)
            .map(|serial_data| serial_data.serial)
            .unwrap_or(0)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(super) fn clear_clipboard_input(&mut self) {
        self.serials.remove(&SerialKind::ClipboardInput);
    }

    #[cfg(any(target_os = "linux", test))]
    pub(super) fn clipboard_serial(&self, focused: bool, same_seat: bool) -> Option<u32> {
        let serial = self.get(SerialKind::ClipboardInput);
        (focused && same_seat && serial != 0).then_some(serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_serial_requires_recent_input_in_the_focused_seat() {
        let mut tracker = SerialTracker::new();
        assert_eq!(tracker.clipboard_serial(true, true), None);
        tracker.update(SerialKind::ClipboardInput, 7);
        assert_eq!(tracker.clipboard_serial(true, true), Some(7));
        assert_eq!(tracker.clipboard_serial(false, true), None);
        assert_eq!(tracker.clipboard_serial(true, false), None);
        tracker.update(SerialKind::ClipboardInput, 8);
        assert_eq!(tracker.clipboard_serial(true, true), Some(8));
        tracker.clear_clipboard_input();
        assert_eq!(tracker.clipboard_serial(true, true), None);
    }
}
