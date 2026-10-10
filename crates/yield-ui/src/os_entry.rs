//! Path-free handoff for native file-open callbacks.

use async_channel::{Receiver, Sender};
use gpui::Global;

pub(crate) const UNSUPPORTED_OPEN_MESSAGE: &str =
    "Opening files from macOS is not supported yet. Add a folder in Yield to browse it.";

/// A bounded signal that records only that macOS offered an open request.
#[derive(Clone)]
pub(crate) struct OsEntrySignal {
    sender: Sender<()>,
    receiver: Receiver<()>,
}

impl OsEntrySignal {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = async_channel::bounded(1);
        Self { sender, receiver }
    }

    /// Discard the native URLs and retain only one path-free pending signal.
    pub(crate) fn receive_open_urls(&self, _urls: Vec<String>) {
        let _ = self.sender.try_send(());
    }

    pub(crate) fn receiver(&self) -> Receiver<()> {
        self.receiver.clone()
    }
}

impl Global for OsEntrySignal {}

#[cfg(test)]
mod tests {
    use super::OsEntrySignal;

    #[gpui::test]
    fn native_open_ingress_coalesces_without_retaining_url_data(cx: &mut gpui::TestAppContext) {
        let signal = OsEntrySignal::new();
        cx.set_global(signal.clone());
        signal.receive_open_urls(vec![String::from(
            "file:///private/example/secret-library/sample.wav",
        )]);
        signal.receive_open_urls(vec![String::from(
            "file:///private/example/another-secret/sample.wav",
        )]);

        let receiver = signal.receiver();
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }
}
