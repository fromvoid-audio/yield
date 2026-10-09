//! Real CPU serializer evidence only; this fixture grants no Source or physical authority.
use super::*;
use reson::recording::{RecordingLimits, RecordingProducer, RecordingSinkBinding};
use std::io::{self, Cursor, Seek, SeekFrom, Write};
use std::time::Duration;

struct Sink(Cursor<Vec<u8>>);
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for Sink {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0.seek(from)
    }
}

#[test]
fn original_terminal_inventory_binds_barrier_and_join_for_every_channel_count() {
    for channels in 1..=8 {
        for frames in [0, 1, 17, 513] {
            let proposal = recording_proposal_for_test(channels, 1024);
            let (binding, mut original) =
                RecordingSinkBinding::bind(Box::new(Sink(Cursor::new(Vec::new()))));
            let (_, foreign) = RecordingSinkBinding::bind(Box::new(Sink(Cursor::new(Vec::new()))));
            let mut producer = RecordingProducer::start(
                binding,
                RecordingLimits::new(proposal.format().sample_rate(), channels, 1024).unwrap(),
            )
            .unwrap();
            producer.submit(&vec![0.25; frames * usize::from(channels)]);
            std::thread::scope(|scope| {
                let stopped = scope.spawn(move || producer.stop().unwrap());
                let barrier = original.recv_barrier(Duration::from_secs(2)).unwrap();
                let terminal = Uuid::new_v4();
                let acquisition = Uuid::new_v4();
                assert!(
                    RecordingFinalizingInventory::capture(
                        &proposal,
                        terminal,
                        acquisition,
                        1,
                        &foreign,
                        &barrier,
                    )
                    .is_err()
                );
                let inventory = RecordingFinalizingInventory::capture(
                    &proposal,
                    terminal,
                    acquisition,
                    1,
                    &original,
                    &barrier,
                )
                .unwrap();
                assert_eq!(inventory.frames, frames as u64);
                let encoded = inventory.encode(&proposal).unwrap();
                assert!(encoded.len() <= CONTROL_BYTES);
                assert_eq!(
                    RecordingFinalizingInventory::decode(&encoded, &proposal).unwrap(),
                    inventory
                );
                for changed in [
                    String::from_utf8(encoded.clone())
                        .unwrap()
                        .replacen("{", "{\"version\":2,", 1),
                    String::from_utf8(encoded.clone()).unwrap().replacen(
                        "{",
                        "{\"unknown\":null,",
                        1,
                    ),
                    format!("{} null", String::from_utf8(encoded.clone()).unwrap()),
                ] {
                    assert!(
                        RecordingFinalizingInventory::decode(changed.as_bytes(), &proposal)
                            .is_err()
                    );
                }
                for field in [
                    "generation",
                    "acquisition",
                    "pending_hash",
                    "pending_write_id",
                    "riff_write_id",
                    "data_write_id",
                    "object_bytes",
                    "unconfirmed",
                ] {
                    let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
                    value[field] = match field {
                        "acquisition" | "pending_write_id" | "riff_write_id" | "data_write_id" => {
                            serde_json::json!(Uuid::nil())
                        }
                        "pending_hash" => serde_json::json!(vec![0; 32]),
                        "generation" => serde_json::json!(0),
                        _ => serde_json::json!(u64::MAX),
                    };
                    let decoded = RecordingFinalizingInventory::decode(
                        &serde_json::to_vec(&value).unwrap(),
                        &proposal,
                    );
                    if field == "pending_hash" {
                        // Historical hash observations are not authenticated until compared
                        // with the same producer's genuine completion below.
                        assert!(decoded.is_ok());
                    } else {
                        assert!(decoded.is_err(), "{field}");
                    }
                }
                barrier.acknowledge().unwrap(); // Codec fixture only, never owner/SQL admission.
                stopped.join().unwrap();
                let joined = original.recv_joined(Duration::from_secs(2)).unwrap();
                inventory
                    .validate_joined(&proposal, &original, &joined)
                    .unwrap();
                assert!(
                    inventory
                        .validate_joined(&proposal, &foreign, &joined)
                        .is_err()
                );
                let mut changed = inventory.clone();
                changed.pending_hash[0] ^= 1;
                assert!(
                    changed
                        .validate_joined(&proposal, &original, &joined)
                        .is_err()
                );
                let mut changed = inventory.clone();
                changed.rejected += 1;
                assert!(
                    changed
                        .validate_joined(&proposal, &original, &joined)
                        .is_err()
                );
            });
        }
    }
}
