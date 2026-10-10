use cpal::{FromSample, SizedSample};
use ringbuf::{HeapCons, HeapProd, traits::*};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub(crate) const SOURCE_CAPACITY: usize = 512;

#[cfg(test)]
const MAX_PROBED_CHANNELS: usize = 64;

/// Fixed-size observation of post-mix callback samples for native acceptance
/// tests. It is absent from production builds and performs only atomic updates.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct CallbackProbe {
    sample_counts: [AtomicU64; MAX_PROBED_CHANNELS],
    peak_bits: [AtomicU32; MAX_PROBED_CHANNELS],
    allocations: AtomicU64,
    deallocations: AtomicU64,
    non_finite_samples: AtomicU64,
    rejected_contributions: AtomicU64,
}

#[cfg(test)]
impl CallbackProbe {
    pub(crate) fn new() -> Self {
        Self {
            sample_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            peak_bits: std::array::from_fn(|_| AtomicU32::new(0)),
            allocations: AtomicU64::new(0),
            deallocations: AtomicU64::new(0),
            non_finite_samples: AtomicU64::new(0),
            rejected_contributions: AtomicU64::new(0),
        }
    }

    fn record(&self, channel: usize, sample: f32) {
        let Some(count) = self.sample_counts.get(channel) else {
            return;
        };
        count.fetch_add(1, Ordering::Relaxed);
        let magnitude = sample.abs();
        if !magnitude.is_finite() {
            self.non_finite_samples.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(peak) = self.peak_bits.get(channel) else {
            return;
        };
        let mut previous = peak.load(Ordering::Relaxed);
        while magnitude.to_bits() > previous {
            match peak.compare_exchange_weak(
                previous,
                magnitude.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(latest) => previous = latest,
            }
        }
    }

    pub(crate) fn sample_count(&self, channel: usize) -> u64 {
        self.sample_counts
            .get(channel)
            .map_or(0, |count| count.load(Ordering::Acquire))
    }

    pub(crate) fn peak(&self, channel: usize) -> f32 {
        self.peak_bits
            .get(channel)
            .map_or(0.0, |peak| f32::from_bits(peak.load(Ordering::Acquire)))
    }

    pub(crate) fn allocation_count(&self) -> u64 {
        self.allocations.load(Ordering::Acquire)
    }

    pub(crate) fn deallocation_count(&self) -> u64 {
        self.deallocations.load(Ordering::Acquire)
    }

    pub(crate) fn non_finite_sample_count(&self) -> u64 {
        self.non_finite_samples.load(Ordering::Acquire)
    }

    pub(crate) fn rejected_contribution_count(&self) -> u64 {
        self.rejected_contributions.load(Ordering::Acquire)
    }

    fn record_callback_churn(&self, allocations: usize, deallocations: usize) {
        self.allocations
            .fetch_add(allocations as u64, Ordering::Relaxed);
        self.deallocations
            .fetch_add(deallocations as u64, Ordering::Relaxed);
    }
}

pub(super) struct SourcePermitPool {
    active: AtomicUsize,
    capacity: usize,
}

impl SourcePermitPool {
    pub(super) const fn new(capacity: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            capacity,
        }
    }

    #[cfg(test)]
    pub(super) fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    fn try_acquire(&'static self, stream_permits: Arc<AtomicUsize>) -> Option<SourcePermit> {
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= self.capacity {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    stream_permits.fetch_add(1, Ordering::AcqRel);
                    return Some(SourcePermit {
                        stream_permits,
                        pool: self,
                    });
                }
                Err(latest) => current = latest,
            }
        }
    }
}

static GLOBAL_SOURCE_PERMITS: SourcePermitPool = SourcePermitPool::new(SOURCE_CAPACITY);

/// A permit follows an accepted source until its Box has been destroyed.
///
/// The source field is declared before the permit so normal field destruction
/// releases capacity only after the source destructor has returned.
pub(crate) struct SourceEntry {
    source: Box<dyn crate::Source + Send>,
    _permit: SourcePermit,
}

impl SourceEntry {
    pub(crate) fn new<S>(source: S, stream_permits: Arc<AtomicUsize>) -> Result<Self, S>
    where
        S: crate::Source + Send + 'static,
    {
        Self::new_with_pool_internal(source, stream_permits, &GLOBAL_SOURCE_PERMITS)
    }

    #[cfg(test)]
    pub(super) fn new_with_pool<S>(
        source: S,
        stream_permits: Arc<AtomicUsize>,
        pool: &'static SourcePermitPool,
    ) -> Result<Self, S>
    where
        S: crate::Source + Send + 'static,
    {
        Self::new_with_pool_internal(source, stream_permits, pool)
    }

    fn new_with_pool_internal<S>(
        source: S,
        stream_permits: Arc<AtomicUsize>,
        pool: &'static SourcePermitPool,
    ) -> Result<Self, S>
    where
        S: crate::Source + Send + 'static,
    {
        let Some(permit) = pool.try_acquire(stream_permits) else {
            return Err(source);
        };
        Ok(Self {
            source: Box::new(source),
            _permit: permit,
        })
    }

    fn next(&mut self) -> Option<f32> {
        self.source.next()
    }

    pub(super) fn last_error(&self) -> Option<String> {
        self.source.last_error()
    }
}

struct SourcePermit {
    stream_permits: Arc<AtomicUsize>,
    pool: &'static SourcePermitPool,
}

impl Drop for SourcePermit {
    fn drop(&mut self) {
        self.stream_permits.fetch_sub(1, Ordering::AcqRel);
        self.pool.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Commands sent to the output dispatcher from control threads.
pub(crate) enum StreamCommand {
    Append {
        generation: u64,
        source: SourceEntry,
        volume: f32,
    },
    Clear {
        generation: u64,
    },
}

struct CallbackSource {
    source: Option<SourceEntry>,
    volume: f32,
    active: bool,
}

impl CallbackSource {
    fn new(source: SourceEntry, volume: f32) -> Self {
        Self {
            source: Some(source),
            volume,
            active: true,
        }
    }
}

/// Fixed-capacity callback-owned mix state. Source ownership leaves this state
/// only through the lock-free retirement producer, including during teardown.
pub(super) struct CallbackState {
    sources: Vec<CallbackSource>,
    command_consumer: HeapCons<StreamCommand>,
    retired_producer: HeapProd<SourceEntry>,
    volume_bits: Arc<AtomicU32>,
    active_sources: Arc<AtomicUsize>,
    clear_pending: Arc<AtomicBool>,
    command_generation: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    current_generation: u64,
    #[cfg(test)]
    output_channels: usize,
    #[cfg(test)]
    probe: Arc<CallbackProbe>,
}

impl CallbackState {
    pub(super) fn new(
        command_consumer: HeapCons<StreamCommand>,
        retired_producer: HeapProd<SourceEntry>,
        volume_bits: Arc<AtomicU32>,
        active_sources: Arc<AtomicUsize>,
        clear_pending: Arc<AtomicBool>,
        command_generation: Arc<AtomicU64>,
        closed: Arc<AtomicBool>,
        #[cfg(test)] output_channels: u16,
        #[cfg(test)] probe: Arc<CallbackProbe>,
    ) -> Self {
        let current_generation = command_generation.load(Ordering::Acquire);
        Self {
            sources: Vec::with_capacity(SOURCE_CAPACITY),
            command_consumer,
            retired_producer,
            volume_bits,
            active_sources,
            clear_pending,
            command_generation,
            closed,
            current_generation,
            #[cfg(test)]
            output_channels: output_channels.max(1) as usize,
            #[cfg(test)]
            probe,
        }
    }

    fn apply_commands(&mut self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            self.apply_clear_generation(self.command_generation.load(Ordering::Acquire));
            self.drain_commands(SOURCE_CAPACITY, true);
            self.retry_retirements();
            self.publish_active_count();
            return true;
        }

        if self.clear_pending.swap(false, Ordering::AcqRel) {
            let generation = self.command_generation.load(Ordering::Acquire);
            self.apply_clear_generation(generation);
        }
        self.drain_commands(SOURCE_CAPACITY, false);
        self.retry_retirements();
        false
    }

    fn drain_commands(&mut self, limit: usize, closing: bool) {
        for _ in 0..limit {
            let command = match self.command_consumer.try_pop() {
                Some(command) => command,
                None => break,
            };
            match command {
                StreamCommand::Append {
                    generation,
                    source,
                    volume,
                } => {
                    if closing || generation < self.current_generation {
                        self.retire_or_retain(source, sanitize_gain(volume));
                        continue;
                    }
                    if generation > self.current_generation {
                        self.apply_clear_generation(generation);
                    }
                    self.sources
                        .push(CallbackSource::new(source, sanitize_gain(volume)));
                }
                StreamCommand::Clear { generation } => {
                    if generation >= self.current_generation {
                        self.apply_clear_generation(generation);
                    }
                }
            }
        }
    }

    fn apply_clear_generation(&mut self, generation: u64) {
        for source in &mut self.sources {
            source.active = false;
        }
        self.current_generation = generation;
        self.retry_retirements();
    }

    fn retry_retirements(&mut self) {
        for slot in &mut self.sources {
            if slot.active {
                continue;
            }
            let Some(source) = slot.source.take() else {
                continue;
            };
            if let Err(source) = self.retired_producer.try_push(source) {
                slot.source = Some(source);
            }
        }
        self.sources.retain(|slot| slot.source.is_some());
    }

    fn retire_or_retain(&mut self, source: SourceEntry, volume: f32) {
        match self.retired_producer.try_push(source) {
            Ok(()) => {}
            Err(source) => self.sources.push(CallbackSource {
                source: Some(source),
                volume,
                active: false,
            }),
        }
    }

    fn publish_active_count(&self) {
        self.active_sources.store(
            self.sources.iter().filter(|source| source.active).count(),
            Ordering::Relaxed,
        );
    }

    #[cfg(test)]
    pub(super) fn process_measured<T>(&mut self, data: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let (allocations, deallocations) =
            crate::test_support::count_thread_allocations_and_deallocations(|| {
                process_audio_callback(self, data);
            });
        self.probe.record_callback_churn(allocations, deallocations);
    }

    fn retire_all_for_drop(&mut self) {
        for source in &mut self.sources {
            source.active = false;
        }
        self.retry_retirements();
        for _ in 0..SOURCE_CAPACITY {
            let Some(command) = self.command_consumer.try_pop() else {
                break;
            };
            if let StreamCommand::Append { source, volume, .. } = command {
                self.retire_or_retain(source, volume);
            }
        }
        self.retry_retirements();
        self.publish_active_count();
    }
}

impl Drop for CallbackState {
    fn drop(&mut self) {
        // CPAL destroys the callback closure only after the lifecycle thread has
        // stopped and dropped the native stream. This is therefore off the device
        // callback; the ring is preferred, with direct destruction as a bounded
        // defensive fallback if the global source-credit invariant is violated.
        self.retire_all_for_drop();
        for slot in &mut self.sources {
            if let Some(source) = slot.source.take() {
                drop(source);
            }
        }
        self.sources.clear();
    }
}

pub(super) fn process_audio_callback<T>(state: &mut CallbackState, data: &mut [T])
where
    T: SizedSample + FromSample<f32>,
    f32: FromSample<T>,
{
    if state.apply_commands() {
        data.fill(T::from_sample(0.0));
        #[cfg(test)]
        for (index, _) in data.iter().enumerate() {
            state.probe.record(index % state.output_channels, 0.0);
        }
        return;
    }

    let master_volume = load_volume(&state.volume_bits);
    for (_index, sample_out) in data.iter_mut().enumerate() {
        let mut mixed = 0.0_f32;
        for slot in &mut state.sources {
            if !slot.active {
                continue;
            }
            let Some(source) = slot.source.as_mut() else {
                slot.active = false;
                continue;
            };
            match source.next() {
                Some(sample) => {
                    let combined_gain = master_volume * slot.volume;
                    let contribution = sample * combined_gain;
                    if contribution.is_finite() {
                        mixed += contribution;
                    } else {
                        #[cfg(test)]
                        state
                            .probe
                            .rejected_contributions
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => slot.active = false,
            }
        }
        let bounded_mixed = if mixed.is_nan() {
            0.0
        } else {
            mixed.clamp(-1.0, 1.0)
        };
        let converted = T::from_sample(bounded_mixed);
        #[cfg(test)]
        state
            .probe
            .record(_index % state.output_channels, converted.to_sample::<f32>());
        *sample_out = converted;
    }
    state.retry_retirements();
    state.publish_active_count();
}

pub(super) fn sanitize_gain(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn load_volume(bits: &AtomicU32) -> f32 {
    f32::from_bits(bits.load(Ordering::Relaxed))
}

pub(super) fn store_volume(bits: &AtomicU32, volume: f32) {
    bits.store(volume.to_bits(), Ordering::Relaxed);
}
