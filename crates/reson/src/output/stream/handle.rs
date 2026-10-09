use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use cpal::traits::StreamTrait;
use ringbuf::{HeapCons, HeapProd, traits::*};
use tracing::warn;

use crate::output::AudioOutputError;
#[cfg(test)]
use crate::output::callback::CallbackProbe;
use crate::output::callback::{
    SOURCE_CAPACITY, SourceEntry, StreamCommand, sanitize_gain, store_volume,
};
use crate::telemetry;

/// Custom handle for a CPAL output stream. The native stream and source reaper
/// live on a lifecycle thread so control-side Drop never joins arbitrary source
/// destructors.
pub struct CpalAudioStream {
    command_sender: SyncSender<StreamCommand>,
    active_sources: Arc<AtomicUsize>,
    volume_bits: Arc<AtomicU32>,
    error_receiver: Receiver<String>,
    clear_pending: Arc<AtomicBool>,
    command_generation: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    admission: Arc<SourceAdmissionGate>,
    stream_permits: Arc<AtomicUsize>,
    dispatcher_thread: Thread,
    #[cfg(test)]
    callback_probe: Arc<CallbackProbe>,
    #[cfg(test)]
    teardown_complete: Arc<AtomicBool>,
}

impl CpalAudioStream {
    /// Start the lifecycle thread, transferring native stream ownership before
    /// the public handle is returned.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        stream: cpal::Stream,
        command_sender: SyncSender<StreamCommand>,
        command_receiver: Receiver<StreamCommand>,
        callback_command_producer: HeapProd<StreamCommand>,
        retired_source_consumer: HeapCons<SourceEntry>,
        error_sender: SyncSender<String>,
        closed: Arc<AtomicBool>,
        active_sources: Arc<AtomicUsize>,
        volume_bits: Arc<AtomicU32>,
        error_receiver: Receiver<String>,
        clear_pending: Arc<AtomicBool>,
        command_generation: Arc<AtomicU64>,
        #[cfg(test)] callback_probe: Arc<CallbackProbe>,
    ) -> Result<Self, AudioOutputError> {
        let admission = Arc::new(SourceAdmissionGate::default());
        let stream_permits = Arc::new(AtomicUsize::new(0));
        let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(1);
        let thread_admission = Arc::clone(&admission);
        let thread_stream_permits = Arc::clone(&stream_permits);
        let thread_closed = Arc::clone(&closed);
        let thread_active_sources = Arc::clone(&active_sources);
        let thread_clear_pending = Arc::clone(&clear_pending);
        let thread_command_generation = Arc::clone(&command_generation);
        #[cfg(test)]
        let teardown_complete = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let thread_teardown_complete = Arc::clone(&teardown_complete);
        let lifecycle = thread::Builder::new()
            .name("audio-stream-lifecycle".to_string())
            .spawn(move || {
                if let Err(source) = stream.play() {
                    let _ = started_sender.send(Err(source));
                    // The callback closure already owns the queues and any
                    // accepted entries. Run the ordinary off-thread shutdown
                    // path even when playback cannot start, so retained
                    // sources receive their last_error/Drop work off RT.
                    thread_admission.close();
                    thread_closed.store(true, Ordering::Release);
                    run_lifecycle(
                        stream,
                        command_receiver,
                        callback_command_producer,
                        retired_source_consumer,
                        error_sender,
                        thread_closed,
                        thread_admission,
                        thread_stream_permits,
                        thread_active_sources,
                        thread_clear_pending,
                        thread_command_generation,
                        #[cfg(test)]
                        thread_teardown_complete,
                    );
                    return;
                }
                let _ = started_sender.send(Ok(()));
                run_lifecycle(
                    stream,
                    command_receiver,
                    callback_command_producer,
                    retired_source_consumer,
                    error_sender,
                    thread_closed,
                    thread_admission,
                    thread_stream_permits,
                    thread_active_sources,
                    thread_clear_pending,
                    thread_command_generation,
                    #[cfg(test)]
                    thread_teardown_complete,
                );
            })
            .map_err(|source| AudioOutputError::LifecycleThread { source })?;
        let dispatcher_thread = lifecycle.thread().clone();
        match started_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                command_sender,
                active_sources,
                volume_bits,
                error_receiver,
                clear_pending,
                command_generation,
                closed,
                admission,
                stream_permits,
                dispatcher_thread,
                #[cfg(test)]
                callback_probe,
                #[cfg(test)]
                teardown_complete,
            }),
            Ok(Err(source)) => Err(AudioOutputError::PlayStream { source }),
            Err(_) => Err(AudioOutputError::LifecycleThread {
                source: std::io::Error::other("audio lifecycle thread exited during startup"),
            }),
        }
    }

    /// Append a source. Admission is reserved before allocating the erased Box.
    /// `Source::next` executes on CPAL's real-time callback thread and must not
    /// block, allocate, perform I/O, or take blocking locks.
    pub fn append_source<S: crate::Source + Send + 'static>(
        &self,
        source: S,
        volume: f32,
    ) -> Result<(), String> {
        let started_at = telemetry::playback_telemetry_enabled().then(Instant::now);
        let Some(_admission) = self.admission.enter() else {
            return Err("Audio output stream is unavailable".to_string());
        };
        let entry =
            SourceEntry::new(source, Arc::clone(&self.stream_permits)).map_err(|source| {
                drop(source);
                "Audio source capacity reached".to_string()
            })?;
        let generation = self.command_generation.load(Ordering::Acquire);
        let result = self
            .command_sender
            .try_send(StreamCommand::Append {
                generation,
                source: entry,
                volume,
            })
            .map_err(|err| match err {
                TrySendError::Full(_) => "Audio command queue full; dropping source".to_string(),
                TrySendError::Disconnected(_) => "Audio output stream is unavailable".to_string(),
            });
        if result.is_ok() {
            self.dispatcher_thread.unpark();
        }
        log_stream_command_timing("append", started_at, result.is_ok());
        result
    }

    /// Replace queued sources with a new source using one callback command.
    pub fn append_source_replacing_previous<S: crate::Source + Send + 'static>(
        &self,
        source: S,
        volume: f32,
    ) -> Result<(), String> {
        let started_at = telemetry::playback_telemetry_enabled().then(Instant::now);
        let Some(_admission) = self.admission.enter() else {
            return Err("Audio output stream is unavailable".to_string());
        };
        let entry = match SourceEntry::new(source, Arc::clone(&self.stream_permits)) {
            Ok(entry) => entry,
            Err(source) => {
                drop(source);
                let generation = self.command_generation.fetch_add(1, Ordering::AcqRel) + 1;
                self.clear_pending.store(true, Ordering::Release);
                let _ = self
                    .command_sender
                    .try_send(StreamCommand::Clear { generation });
                self.dispatcher_thread.unpark();
                return Err("Audio source capacity reached".to_string());
            }
        };
        let generation = self.command_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let result = self
            .command_sender
            .try_send(StreamCommand::Append {
                generation,
                source: entry,
                volume,
            })
            .map_err(|err| match err {
                TrySendError::Full(_) => {
                    self.clear_pending.store(true, Ordering::Release);
                    "Audio command queue full; dropping replacement source with clear pending"
                        .to_string()
                }
                TrySendError::Disconnected(_) => "Audio output stream is unavailable".to_string(),
            });
        if result.is_ok() || self.clear_pending.load(Ordering::Acquire) {
            self.dispatcher_thread.unpark();
        }
        log_stream_command_timing("replace_append", started_at, result.is_ok());
        result
    }

    /// Clear all queued sources on the audio thread.
    pub fn clear_sources(&self) -> Result<(), String> {
        let started_at = telemetry::playback_telemetry_enabled().then(Instant::now);
        let result = request_clear(
            &self.command_sender,
            &self.clear_pending,
            &self.command_generation,
        )
        .map_err(|err| match err {
            TrySendError::Full(_) => "Audio command queue full; clear pending".to_string(),
            TrySendError::Disconnected(_) => "Audio output stream is unavailable".to_string(),
        });
        self.dispatcher_thread.unpark();
        log_stream_command_timing("clear", started_at, result.is_ok());
        result
    }

    /// Update the master output volume used by the audio callback.
    pub fn set_volume(&self, volume: f32) {
        store_volume(&self.volume_bits, sanitize_gain(volume));
    }

    /// Return the last known count of active sources.
    pub fn active_source_count(&self) -> usize {
        self.active_sources.load(Ordering::Relaxed)
    }

    /// Return and clear the most recent audio error, if any.
    pub fn take_error(&self) -> Option<String> {
        self.error_receiver.try_iter().last()
    }

    #[cfg(test)]
    pub(crate) fn callback_probe(&self) -> Arc<CallbackProbe> {
        Arc::clone(&self.callback_probe)
    }

    #[cfg(test)]
    pub(crate) fn teardown_complete(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.teardown_complete)
    }

    /// Create a monitor sink that sends sources into this stream.
    pub fn monitor_sink(&self, volume: f32) -> MonitorSink {
        MonitorSink {
            command_sender: self.command_sender.clone(),
            clear_pending: Arc::clone(&self.clear_pending),
            command_generation: Arc::clone(&self.command_generation),
            admission: Arc::clone(&self.admission),
            stream_permits: Arc::clone(&self.stream_permits),
            dispatcher_thread: self.dispatcher_thread.clone(),
            volume,
        }
    }
}

impl Drop for CpalAudioStream {
    fn drop(&mut self) {
        self.admission.close();
        self.closed.store(true, Ordering::Release);
        self.dispatcher_thread.unpark();
    }
}

#[derive(Default)]
struct SourceAdmissionGate {
    closed: AtomicBool,
    in_flight: AtomicUsize,
}

impl SourceAdmissionGate {
    fn enter(self: &Arc<Self>) -> Option<SourceAdmission> {
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(SourceAdmission(Arc::clone(self)))
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

struct SourceAdmission(Arc<SourceAdmissionGate>);

impl Drop for SourceAdmission {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_lifecycle(
    stream: cpal::Stream,
    command_receiver: Receiver<StreamCommand>,
    mut callback_command_producer: HeapProd<StreamCommand>,
    mut retired_source_consumer: HeapCons<SourceEntry>,
    error_sender: SyncSender<String>,
    closed: Arc<AtomicBool>,
    admission: Arc<SourceAdmissionGate>,
    stream_permits: Arc<AtomicUsize>,
    _active_sources: Arc<AtomicUsize>,
    clear_pending: Arc<AtomicBool>,
    _command_generation: Arc<AtomicU64>,
    #[cfg(test)] teardown_complete: Arc<AtomicBool>,
) {
    let mut stream = Some(stream);
    let mut pending_callback_command = None;
    let mut pending_retirements = Vec::with_capacity(SOURCE_CAPACITY);
    loop {
        while let Some(source) = retired_source_consumer.try_pop() {
            dispose_source(source, &error_sender);
        }

        if closed.load(Ordering::Acquire) {
            while admission.in_flight.load(Ordering::SeqCst) != 0 {
                thread::park_timeout(Duration::from_millis(1));
            }

            if let Some(command) = pending_callback_command.take() {
                retire_control_command(command, &mut pending_retirements);
            }
            loop {
                match command_receiver.try_recv() {
                    Ok(command) => {
                        retire_control_command(command, &mut pending_retirements);
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            drop(command_receiver);
            if let Some(native_stream) = stream.take() {
                let _ = native_stream.pause();
                drop(native_stream);
            }

            for source in pending_retirements.drain(..) {
                dispose_source(source, &error_sender);
            }
            loop {
                while let Some(source) = retired_source_consumer.try_pop() {
                    dispose_source(source, &error_sender);
                }
                if stream_permits.load(Ordering::Acquire) == 0 {
                    #[cfg(test)]
                    teardown_complete.store(true, Ordering::Release);
                    return;
                }
                // A source destructor may have blocked the callback's final
                // retirement progress. The native stream is already stopped;
                // keep draining until every accepted source has actually died.
                thread::park_timeout(Duration::from_millis(1));
            }
        }

        let mut worked = false;
        if let Some(command) = pending_callback_command.take() {
            match callback_command_producer.try_push(command) {
                Ok(()) => worked = true,
                Err(command) => pending_callback_command = Some(command),
            }
        }
        if pending_callback_command.is_none() {
            for _ in 0..64 {
                match command_receiver.try_recv() {
                    Ok(StreamCommand::Clear { generation }) => {
                        match callback_command_producer
                            .try_push(StreamCommand::Clear { generation })
                        {
                            Ok(()) => worked = true,
                            Err(_) => {
                                // The callback observes the generation through
                                // clear_pending even if the advisory ring is full.
                                clear_pending.store(true, Ordering::Release);
                            }
                        }
                    }
                    Ok(command @ StreamCommand::Append { .. }) => {
                        match callback_command_producer.try_push(command) {
                            Ok(()) => worked = true,
                            Err(command) => {
                                pending_callback_command = Some(command);
                                break;
                            }
                        }
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
        if !worked {
            thread::park_timeout(Duration::from_millis(1));
        }
    }
}

fn retire_control_command(command: StreamCommand, pending_retirements: &mut Vec<SourceEntry>) {
    if let StreamCommand::Append { source, .. } = command {
        if pending_retirements.len() < SOURCE_CAPACITY {
            pending_retirements.push(source);
        } else {
            // This path is off the device callback. Preserve ownership cleanup
            // even if the credit invariant is broken.
            drop(source);
        }
    }
}

fn dispose_source(source: SourceEntry, error_sender: &SyncSender<String>) {
    if let Some(error) = source.last_error() {
        let _ = error_sender.try_send(error);
    }
    drop(source);
}

/// A bridge for input monitoring that mimics a Sink-like interface.
#[derive(Clone)]
pub struct MonitorSink {
    command_sender: SyncSender<StreamCommand>,
    clear_pending: Arc<AtomicBool>,
    command_generation: Arc<AtomicU64>,
    admission: Arc<SourceAdmissionGate>,
    stream_permits: Arc<AtomicUsize>,
    dispatcher_thread: Thread,
    /// Gain applied to appended sources.
    pub volume: f32,
}

impl MonitorSink {
    /// Append a source. `Source::next` executes on CPAL's real-time callback.
    pub fn append<S: crate::Source + Send + 'static>(&self, source: S) {
        let Some(_admission) = self.admission.enter() else {
            return;
        };
        let entry = match SourceEntry::new(source, Arc::clone(&self.stream_permits)) {
            Ok(entry) => entry,
            Err(source) => {
                drop(source);
                warn!("Failed to append monitor source: global source capacity reached");
                return;
            }
        };
        let generation = self.command_generation.load(Ordering::Acquire);
        if let Err(err) = self.command_sender.try_send(StreamCommand::Append {
            generation,
            source: entry,
            volume: self.volume,
        }) {
            match err {
                TrySendError::Full(_) => {
                    warn!("Failed to append monitor source: command queue full");
                }
                TrySendError::Disconnected(_) => {
                    warn!("Failed to append monitor source: output stream unavailable");
                }
            }
        } else {
            self.dispatcher_thread.unpark();
        }
    }

    /// Begin playback (no-op for the monitor sink).
    pub fn play(&self) {}

    /// Stop playback by clearing queued sources.
    pub fn stop(&self) {
        if let Err(err) = request_clear(
            &self.command_sender,
            &self.clear_pending,
            &self.command_generation,
        ) {
            match err {
                TrySendError::Full(_) => {
                    warn!("Failed to stop monitor sink: command queue full; clear pending");
                }
                TrySendError::Disconnected(_) => {
                    warn!("Failed to stop monitor sink: output stream unavailable");
                }
            }
        }
        self.dispatcher_thread.unpark();
    }
}

fn request_clear(
    command_sender: &SyncSender<StreamCommand>,
    clear_pending: &AtomicBool,
    command_generation: &AtomicU64,
) -> Result<(), TrySendError<StreamCommand>> {
    let generation = command_generation.fetch_add(1, Ordering::AcqRel) + 1;
    clear_pending.store(true, Ordering::Release);
    command_sender.try_send(StreamCommand::Clear { generation })
}

fn log_stream_command_timing(command: &'static str, started_at: Option<Instant>, submitted: bool) {
    let Some(started_at) = started_at else {
        return;
    };
    tracing::info!(
        target: "perf::audio_start",
        module = "reson_stream",
        stage = "stream_command_try_send",
        command,
        submitted,
        elapsed_ms = telemetry::elapsed_ms(started_at.elapsed()),
        "Audio player stage"
    );
}

#[cfg(test)]
pub(crate) fn monitor_sink_for_tests(
    command_sender: SyncSender<StreamCommand>,
    clear_pending: Arc<AtomicBool>,
    command_generation: Arc<AtomicU64>,
) -> MonitorSink {
    MonitorSink {
        command_sender,
        clear_pending,
        command_generation,
        admission: Arc::new(SourceAdmissionGate::default()),
        stream_permits: Arc::new(AtomicUsize::new(0)),
        dispatcher_thread: thread::current(),
        volume: 1.0,
    }
}

#[cfg(test)]
mod admission_tests {
    use super::SourceAdmissionGate;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn close_waits_for_racing_admissions_and_rejects_later_entries() {
        const RACERS: usize = 16;
        let gate = Arc::new(SourceAdmissionGate::default());
        let start = Arc::new(Barrier::new(RACERS + 1));
        let racers = (0..RACERS)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    for _ in 0..256 {
                        drop(gate.enter());
                    }
                })
            })
            .collect::<Vec<_>>();

        start.wait();
        gate.close();
        for racer in racers {
            racer.join().unwrap();
        }

        assert_eq!(gate.in_flight.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(gate.enter().is_none());
    }
}
