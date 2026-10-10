use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::mpsc::{self, Receiver, SyncSender};

use cpal::traits::DeviceTrait;
use cpal::{FromSample, SizedSample};
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};
use tracing::warn;

#[cfg(test)]
use crate::output::callback::CallbackProbe;
#[cfg(not(test))]
use crate::output::callback::process_audio_callback;
use crate::output::callback::{CallbackState, SOURCE_CAPACITY, SourceEntry, StreamCommand};

const STREAM_ERROR_CAPACITY: usize = 16;

/// Raw stream construction pieces returned before wrapping in `CpalAudioStream`.
pub(super) struct BuiltStreamState {
    pub(super) stream: cpal::Stream,
    pub(super) command_sender: SyncSender<StreamCommand>,
    pub(super) command_receiver: Receiver<StreamCommand>,
    pub(super) callback_command_producer: HeapProd<StreamCommand>,
    pub(super) retired_source_consumer: HeapCons<SourceEntry>,
    pub(super) error_receiver: Receiver<String>,
    pub(super) error_sender: mpsc::SyncSender<String>,
}

pub(super) fn build_stream_with_state(
    device: &cpal::Device,
    stream_config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    volume_bits: Arc<AtomicU32>,
    active_sources: Arc<AtomicUsize>,
    clear_pending: Arc<AtomicBool>,
    command_generation: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    #[cfg(test)] callback_probe: Arc<CallbackProbe>,
) -> Result<BuiltStreamState, cpal::BuildStreamError> {
    const COMMAND_QUEUE_CAPACITY: usize = 512;
    let (command_sender, command_receiver) = mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
    let (error_sender, error_receiver) = mpsc::sync_channel(STREAM_ERROR_CAPACITY);
    let stream_error_sender = error_sender.clone();
    let (callback_command_producer, command_consumer) =
        HeapRb::<StreamCommand>::new(SOURCE_CAPACITY).split();
    let (retired_source_producer, retired_source_consumer) =
        HeapRb::<SourceEntry>::new(SOURCE_CAPACITY).split();
    let callback_state = CallbackState::new(
        command_consumer,
        retired_source_producer,
        volume_bits,
        active_sources,
        clear_pending.clone(),
        command_generation.clone(),
        closed.clone(),
        #[cfg(test)]
        stream_config.channels,
        #[cfg(test)]
        callback_probe,
    );
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_typed_output_stream::<f32>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::F64 => build_typed_output_stream::<f64>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::I8 => build_typed_output_stream::<i8>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::I16 => build_typed_output_stream::<i16>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::I24 => build_typed_output_stream::<cpal::I24>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::I32 => build_typed_output_stream::<i32>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::I64 => build_typed_output_stream::<i64>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::U8 => build_typed_output_stream::<u8>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::U16 => build_typed_output_stream::<u16>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::U24 => build_typed_output_stream::<cpal::U24>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::U32 => build_typed_output_stream::<u32>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        cpal::SampleFormat::U64 => build_typed_output_stream::<u64>(
            device,
            stream_config,
            callback_state,
            stream_error_sender.clone(),
        )?,
        format => {
            warn!("Unsupported output sample format {format:?}; trying f32 stream");
            build_typed_output_stream::<f32>(
                device,
                stream_config,
                callback_state,
                stream_error_sender.clone(),
            )?
        }
    };
    Ok(BuiltStreamState {
        stream,
        command_sender,
        command_receiver,
        callback_command_producer,
        retired_source_consumer,
        error_receiver,
        error_sender,
    })
}

fn build_typed_output_stream<T>(
    device: &cpal::Device,
    stream_config: &cpal::StreamConfig,
    mut callback_state: CallbackState,
    stream_error_sender: mpsc::SyncSender<String>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
    f32: FromSample<T>,
{
    device.build_output_stream(
        stream_config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            #[cfg(test)]
            callback_state.process_measured(data);
            #[cfg(not(test))]
            process_audio_callback(&mut callback_state, data);
        },
        move |err| {
            let message = format!("Audio output stream error: {err}");
            tracing::error!("{message}");
            // Keep error custody bounded even if the UI never polls it. Error
            // callbacks are not the data callback; never block stream error
            // delivery on a full consumer queue.
            let _ = stream_error_sender.try_send(message);
        },
        None,
    )
}
