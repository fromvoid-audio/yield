use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    RootRelativePath, SourceDbError, SourceRevision, SourceWriter, SourceWriterError,
    map_sql_error, normalize_relative_path,
};
use crate::sample_sources::SourcePlaymarkGridTempo;
use crate::sample_sources::readiness::{SourceFeatureInput, SourceFeatureInputError};

/// An authored playmark span and the audio dimensions used to derive its tempo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePlaymarkTempo {
    first_frame: u64,
    end_frame: u64,
    source_frames: u64,
    sample_rate: u32,
    beat_count: u8,
}

impl SourcePlaymarkTempo {
    /// Construct a nonempty frame range within the supplied audio dimensions.
    pub fn new(
        first_frame: u64,
        end_frame: u64,
        source_frames: u64,
        sample_rate: u32,
        beat_count: u8,
    ) -> Option<Self> {
        (first_frame < end_frame
            && end_frame <= source_frames
            && i64::try_from(source_frames).is_ok()
            && sample_rate > 0
            && beat_count > 0)
            .then_some(Self {
                first_frame,
                end_frame,
                source_frames,
                sample_rate,
                beat_count,
            })
    }

    /// Inclusive start and exclusive end frame.
    pub fn frame_range(self) -> (u64, u64) {
        (self.first_frame, self.end_frame)
    }

    /// Total frames, native sample rate and configured beat count.
    pub fn dimensions(self) -> (u64, u32, u8) {
        (self.source_frames, self.sample_rate, self.beat_count)
    }

    /// Compute BPM from the persisted span without retaining a second rounded value.
    pub fn bpm(self) -> f64 {
        f64::from(self.beat_count) * 60.0 * f64::from(self.sample_rate)
            / (self.end_frame - self.first_frame) as f64
    }
}

impl SourceWriter {
    /// Read the authored playmark for the current audited file generation.
    pub fn read_playmark_tempo(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
    ) -> Result<Option<SourcePlaymarkTempo>, SourceWriterError> {
        self.read_playmark_value(expected, relative, read_tempo)
    }

    /// Read the explicit grid tempo for the exact audited file.
    pub fn read_playmark_grid_tempo(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
    ) -> Result<Option<SourcePlaymarkGridTempo>, SourceWriterError> {
        self.read_playmark_value(expected, relative, read_grid_tempo)
    }

    fn read_playmark_value<T>(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
        read: impl FnOnce(
            &Connection,
            &str,
            &SourceFeatureInput,
        ) -> Result<Option<T>, SourceWriterError>,
    ) -> Result<Option<T>, SourceWriterError> {
        let input = self.hold_playmark_tempo_input(expected, relative)?;
        let path = normalize_relative_path(relative.as_path())?;
        let tempo = read(&self.database.connection, &path, &input)?;
        input.validate()?;
        Ok(tempo)
    }

    /// Replace or clear the entire authored span only if its expected prior value still matches.
    ///
    /// The submitted dimensions describe the caller's loaded audio. The Source owner binds them
    /// to the exact audited media identity and scanner digest. A conflict returns false without
    /// changing the record. The manifest revision is unaffected.
    pub fn compare_and_set_playmark_tempo(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
        previous: Option<SourcePlaymarkTempo>,
        next: Option<SourcePlaymarkTempo>,
        cancel: &AtomicBool,
    ) -> Result<bool, SourceWriterError> {
        self.compare_and_set_playmark_value(
            expected,
            relative,
            (previous, next),
            cancel,
            read_tempo,
            write_tempo,
        )
    }

    /// Replace or clear the exact file's explicit grid tempo without changing its range.
    ///
    /// A conflict returns false. The content binding, transaction and cancellation follow the
    /// same policy as the authored range, and neither operation advances the manifest revision.
    pub fn compare_and_set_playmark_grid_tempo(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
        previous: Option<SourcePlaymarkGridTempo>,
        next: Option<SourcePlaymarkGridTempo>,
        cancel: &AtomicBool,
    ) -> Result<bool, SourceWriterError> {
        self.compare_and_set_playmark_value(
            expected,
            relative,
            (previous, next),
            cancel,
            read_grid_tempo,
            write_grid_tempo,
        )
    }

    fn compare_and_set_playmark_value<T: Eq>(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
        change: (Option<T>, Option<T>),
        cancel: &AtomicBool,
        read: impl FnOnce(
            &Connection,
            &str,
            &SourceFeatureInput,
        ) -> Result<Option<T>, SourceWriterError>,
        write: impl FnOnce(
            &Connection,
            &str,
            &SourceFeatureInput,
            Option<T>,
        ) -> Result<(), SourceWriterError>,
    ) -> Result<bool, SourceWriterError> {
        let (previous, next) = change;
        if cancel.load(Ordering::Acquire) {
            return Err(SourceFeatureInputError::Cancelled.into());
        }
        self.database.validate_profile_owned_mutation()?;
        let input = self.hold_playmark_tempo_input(expected, relative)?;
        let path = normalize_relative_path(relative.as_path())?;
        let batch = self.database.write_batch()?;
        let revision = batch.get_revision()?;
        if revision != expected.revision() {
            return Err(SourceWriterError::StaleRevision {
                expected: expected.revision(),
                actual: revision,
            });
        }
        let tx = batch.curation_transaction();
        if read(tx, &path, &input)? != previous {
            return Ok(false);
        }
        if previous == next {
            return Ok(true);
        }
        crate::sqlite_wal::enforce_wal_admission(tx).map_err(SourceDbError::from)?;
        write(tx, &path, &input, next)?;
        input.validate()?;
        if cancel.load(Ordering::Acquire) {
            return Err(SourceFeatureInputError::Cancelled.into());
        }
        batch.commit_auxiliary_state()?;
        Ok(true)
    }
}

fn write_tempo(
    tx: &Connection,
    path: &str,
    input: &SourceFeatureInput,
    next: Option<SourcePlaymarkTempo>,
) -> Result<(), SourceWriterError> {
    let hash = audited_digest(input)?;
    if let Some(tempo) = next {
        tx.execute(
            "INSERT INTO source_playmark_tempo (
                    path, file_identity, content_hash, first_frame, end_frame, source_frames,
                    sample_rate, beat_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(path) DO UPDATE SET
                    file_identity = excluded.file_identity, content_hash = excluded.content_hash,
                    first_frame = excluded.first_frame, end_frame = excluded.end_frame,
                    source_frames = excluded.source_frames, sample_rate = excluded.sample_rate,
                    beat_count = excluded.beat_count",
            params![
                path,
                input.identity,
                hash,
                tempo.first_frame,
                tempo.end_frame,
                tempo.source_frames,
                tempo.sample_rate,
                tempo.beat_count
            ],
        )
        .map_err(map_sql_error)?;
    } else {
        tx.execute("DELETE FROM source_playmark_tempo WHERE path = ?1", [&path])
            .map_err(map_sql_error)?;
    }
    Ok(())
}

fn audited_digest(input: &SourceFeatureInput) -> Result<&str, SourceWriterError> {
    let hash = input.content_hash()?;
    if !hash
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(SourceWriterError::InvalidRequest(
            "invalid audited content digest",
        ));
    }
    Ok(hash)
}

fn read_tempo(
    connection: &Connection,
    path: &str,
    input: &SourceFeatureInput,
) -> Result<Option<SourcePlaymarkTempo>, SourceWriterError> {
    let hash = audited_digest(input)?;
    let fields = connection
        .query_row(
            "SELECT first_frame, end_frame, source_frames, sample_rate, beat_count
             FROM source_playmark_tempo
             WHERE path = ?1 AND file_identity = ?2 AND content_hash = ?3",
            params![path, input.identity, hash],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql_error)?;
    fields
        .map(|(first, end, total, rate, beats)| {
            SourcePlaymarkTempo::new(first, end, total, rate, beats).ok_or(
                SourceWriterError::InvalidRequest("invalid stored playmark tempo"),
            )
        })
        .transpose()
}

fn read_grid_tempo(
    connection: &Connection,
    path: &str,
    input: &SourceFeatureInput,
) -> Result<Option<SourcePlaymarkGridTempo>, SourceWriterError> {
    let hash = audited_digest(input)?;
    connection
        .query_row(
            "SELECT milli_bpm FROM source_playmark_grid_tempo
             WHERE path = ?1 AND file_identity = ?2 AND content_hash = ?3",
            params![path, input.identity, hash],
            |row| row.get::<_, u32>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .map(|value| {
            SourcePlaymarkGridTempo::from_milli_bpm(value).ok_or(SourceWriterError::InvalidRequest(
                "invalid stored playmark grid tempo",
            ))
        })
        .transpose()
}

fn write_grid_tempo(
    connection: &Connection,
    path: &str,
    input: &SourceFeatureInput,
    next: Option<SourcePlaymarkGridTempo>,
) -> Result<(), SourceWriterError> {
    let hash = audited_digest(input)?;
    if let Some(tempo) = next {
        connection.execute(
            "INSERT INTO source_playmark_grid_tempo (path, file_identity, content_hash, milli_bpm)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET file_identity = excluded.file_identity,
             content_hash = excluded.content_hash, milli_bpm = excluded.milli_bpm",
            params![path, input.identity, hash, tempo.milli_bpm()],
        ).map_err(map_sql_error)?;
    } else {
        connection
            .execute(
                "DELETE FROM source_playmark_grid_tempo WHERE path = ?1",
                [path],
            )
            .map_err(map_sql_error)?;
    }
    Ok(())
}
