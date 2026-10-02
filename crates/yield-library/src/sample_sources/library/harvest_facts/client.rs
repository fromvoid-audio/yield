//! Read-only cloneable facade over the existing retained global command transport.
use super::super::{
    GlobalLibraryWriterQueueError, ProfileAcquisitionId, ProfileTagVocabularyClient,
};
use super::*;
use std::sync::mpsc::Receiver;
/// Cloneable bounded Harvest read client. It keeps no writer/profile lock alive.
/// The private transport shares the actual owner's admission gate and closing state.
#[derive(Clone)]
pub struct HarvestFactsClient {
    queue: ProfileTagVocabularyClient,
}
impl HarvestFactsClient {
    pub(in super::super) fn from_queue(queue: ProfileTagVocabularyClient) -> Self {
        Self { queue }
    }
    /// Actual captured global acquisition; correlation remains distinct from live authority.
    pub fn acquisition_id(&self) -> ProfileAcquisitionId {
        self.queue.acquisition_id()
    }
    /// Forward a privately source-backed bounded read onto the same retained global worker.
    pub fn read(
        &self,
        request: HarvestFactsRequest,
    ) -> Result<Receiver<Result<HarvestFactsBatch, LibraryError>>, GlobalLibraryWriterQueueError>
    {
        self.queue.read_harvest_facts(request)
    }
}
