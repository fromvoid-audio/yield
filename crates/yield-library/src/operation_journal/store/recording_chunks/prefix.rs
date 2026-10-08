//! Original scalar PCM/header prefix rules shared by both immutable readers.
use super::*;

pub(super) struct State {
    next_pcm: u32,
    next_header: u32,
    previous_pcm: Option<i64>,
    previous_header: Option<i64>,
    first_pcm_sequence: Option<i64>,
    final_header_sequence: Option<i64>,
    initial_header_sequence: Option<i64>,
    previous_pcm_full: bool,
}
impl State {
    pub(super) fn new() -> Self {
        Self {
            next_pcm: 0,
            next_header: 0,
            previous_pcm: None,
            previous_header: None,
            first_pcm_sequence: None,
            final_header_sequence: None,
            initial_header_sequence: None,
            previous_pcm_full: true,
        }
    }
    pub(super) fn observe(
        &mut self,
        sequence: i64,
        chunk: &RecordingCapturedChunk,
        proposal: &RecordingProposal,
    ) -> Result<(), OperationJournalStoreError> {
        match chunk.role() {
            RecordingChunkRole::Pcm { ordinal } => {
                if ordinal != self.next_pcm
                    || !self.previous_pcm_full
                    || self.previous_pcm.is_some_and(|n| sequence <= n)
                {
                    return Err(error("restart recording PCM prefix has an ordering gap"));
                }
                self.first_pcm_sequence.get_or_insert(sequence);
                self.previous_pcm = Some(sequence);
                self.previous_pcm_full =
                    chunk.bytes().len() as u64 == proposal.format().pcm_chunk_bytes();
                self.next_pcm += 1;
            }
            RecordingChunkRole::Header { revision } => {
                if revision != self.next_header
                    || self.previous_header.is_some_and(|n| sequence <= n)
                {
                    return Err(error("restart recording header prefix has an ordering gap"));
                }
                if revision == 0 {
                    self.initial_header_sequence = Some(sequence);
                } else {
                    self.final_header_sequence.get_or_insert(sequence);
                }
                self.previous_header = Some(sequence);
                self.next_header += 1;
            }
        }
        Ok(())
    }
    pub(super) fn finish(self) -> Result<(), OperationJournalStoreError> {
        if self.first_pcm_sequence.is_some_and(|pcm| {
            self.initial_header_sequence
                .is_none_or(|header| pcm <= header)
        }) || self.previous_pcm.is_some_and(|pcm| {
            self.final_header_sequence
                .is_some_and(|header| pcm >= header)
        }) {
            return Err(error(
                "restart recording capture crosses its header boundary",
            ));
        }
        Ok(())
    }
}
