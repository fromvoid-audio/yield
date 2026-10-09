//! Process-private CoreAudio fixtures for explicitly enabled native tests.
//!
//! This module is included only by macOS unit tests. It deliberately has no
//! property setters: source device formats and the system default are observed
//! before a fixture is created and checked after its owned aggregate goes away.

#![allow(dead_code)]

use std::ffi::{CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

type AudioObjectId = u32;
type CfTypeRef = *const c_void;
type CfStringRef = CfTypeRef;
type CfIndex = isize;

const AUDIO_OBJECT_SYSTEM: AudioObjectId = 1;
const AUDIO_OBJECT_UNKNOWN: AudioObjectId = 0;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const SCOPE_OUTPUT: u32 = fourcc(b"outp");
const ELEMENT_MAIN: u32 = 0;
const PROP_DEVICES: u32 = fourcc(b"dev#");
const PROP_DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const PROP_DEVICE_UID: u32 = fourcc(b"uid ");
const PROP_NOMINAL_RATE: u32 = fourcc(b"nsrt");
const PROP_STREAM_CONFIGURATION: u32 = fourcc(b"slay");
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT32: CfIndex = 3;
const DESTROY_WAIT: Duration = Duration::from_secs(2);

const fn fourcc(bytes: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*bytes)
}

#[repr(C)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectId,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
    ) -> i32;
    fn AudioObjectGetPropertyData(
        object: AudioObjectId,
        address: *const AudioObjectPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioHardwareCreateAggregateDevice(
        description: CfTypeRef,
        device: *mut AudioObjectId,
    ) -> i32;
    fn AudioHardwareDestroyAggregateDevice(device: AudioObjectId) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: CfTypeRef);
    fn CFStringCreateWithCString(
        allocator: CfTypeRef,
        value: *const c_char,
        encoding: u32,
    ) -> CfStringRef;
    fn CFStringGetCString(
        value: CfStringRef,
        buffer: *mut c_char,
        buffer_size: CfIndex,
        encoding: u32,
    ) -> bool;
    fn CFNumberCreate(
        allocator: CfTypeRef,
        number_type: CfIndex,
        value: *const c_void,
    ) -> CfTypeRef;
    fn CFDictionaryCreate(
        allocator: CfTypeRef,
        keys: *const CfTypeRef,
        values: *const CfTypeRef,
        count: CfIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfTypeRef;
    fn CFArrayCreate(
        allocator: CfTypeRef,
        values: *const CfTypeRef,
        count: CfIndex,
        callbacks: *const c_void,
    ) -> CfTypeRef;
    fn CFUUIDCreate(allocator: CfTypeRef) -> CfTypeRef;
    fn CFUUIDCreateString(allocator: CfTypeRef, uuid: CfTypeRef) -> CfStringRef;

    // These exported callback tables are passed by address; their C layouts
    // are intentionally opaque because CoreFoundation consumes them.
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    static kCFTypeArrayCallBacks: u8;
}

struct CfOwned(CfTypeRef);

impl CfOwned {
    fn new(value: CfTypeRef, what: &str) -> Result<Self, String> {
        if value.is_null() {
            Err(format!("CoreFoundation could not create {what}"))
        } else {
            Ok(Self(value))
        }
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cf_string(value: &str) -> Result<CfOwned, String> {
    let value = CString::new(value).map_err(|_| "CoreAudio identifier contains NUL".to_owned())?;
    let string =
        unsafe { CFStringCreateWithCString(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8) };
    CfOwned::new(string, "string")
}

fn cf_number(value: i32) -> Result<CfOwned, String> {
    CfOwned::new(
        unsafe { CFNumberCreate(ptr::null(), CF_NUMBER_SINT32, (&value as *const i32).cast()) },
        "number",
    )
}

fn cf_dictionary(entries: &[(&str, CfOwned)]) -> Result<CfOwned, String> {
    let keys = entries
        .iter()
        .map(|(key, _)| cf_string(key))
        .collect::<Result<Vec<_>, _>>()?;
    let key_refs = keys.iter().map(|value| value.0).collect::<Vec<_>>();
    let value_refs = entries.iter().map(|(_, value)| value.0).collect::<Vec<_>>();
    let dictionary = unsafe {
        CFDictionaryCreate(
            ptr::null(),
            key_refs.as_ptr(),
            value_refs.as_ptr(),
            entries.len() as CfIndex,
            (&kCFTypeDictionaryKeyCallBacks as *const u8).cast(),
            (&kCFTypeDictionaryValueCallBacks as *const u8).cast(),
        )
    };
    CfOwned::new(dictionary, "dictionary")
}

fn cf_array(values: &[CfOwned]) -> Result<CfOwned, String> {
    let refs = values.iter().map(|value| value.0).collect::<Vec<_>>();
    CfOwned::new(
        unsafe {
            CFArrayCreate(
                ptr::null(),
                refs.as_ptr(),
                refs.len() as CfIndex,
                (&kCFTypeArrayCallBacks as *const u8).cast(),
            )
        },
        "array",
    )
}

fn unique_uuid() -> Result<String, String> {
    let uuid = CfOwned::new(unsafe { CFUUIDCreate(ptr::null()) }, "UUID")?;
    let string = CfOwned::new(
        unsafe { CFUUIDCreateString(ptr::null(), uuid.0) },
        "UUID string",
    )?;
    let mut buffer = [0_i8; 64];
    let converted = unsafe {
        CFStringGetCString(
            string.0,
            buffer.as_mut_ptr(),
            buffer.len() as CfIndex,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if !converted {
        return Err("CoreFoundation could not format UUID".into());
    }
    let value = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "CoreFoundation returned a non-UTF8 UUID".into())
}

fn property_address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        selector,
        scope,
        element: ELEMENT_MAIN,
    }
}

fn property_data<T: Copy>(object: AudioObjectId, selector: u32, scope: u32) -> Result<T, String> {
    let address = property_address(selector, scope);
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            value.as_mut_ptr().cast(),
        )
    };
    if status != 0 || size as usize != std::mem::size_of::<T>() {
        return Err(format!("CoreAudio property read failed (status {status})"));
    }
    Ok(unsafe { value.assume_init() })
}

fn device_ids() -> Result<Vec<AudioObjectId>, String> {
    let address = property_address(PROP_DEVICES, SCOPE_GLOBAL);
    let mut byte_size = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            AUDIO_OBJECT_SYSTEM,
            &address,
            0,
            ptr::null(),
            &mut byte_size,
        )
    };
    if status != 0 || byte_size as usize % std::mem::size_of::<AudioObjectId>() != 0 {
        return Err(format!(
            "CoreAudio device enumeration failed (status {status})"
        ));
    }
    let mut devices = vec![0; byte_size as usize / std::mem::size_of::<AudioObjectId>()];
    let mut returned_size = byte_size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            AUDIO_OBJECT_SYSTEM,
            &address,
            0,
            ptr::null(),
            &mut returned_size,
            devices.as_mut_ptr().cast(),
        )
    };
    if status != 0
        || returned_size as usize > byte_size as usize
        || returned_size as usize % std::mem::size_of::<AudioObjectId>() != 0
    {
        return Err(format!(
            "CoreAudio device enumeration failed (status {status})"
        ));
    }
    devices.truncate(returned_size as usize / std::mem::size_of::<AudioObjectId>());
    Ok(devices)
}

fn string_property(object: AudioObjectId, selector: u32) -> Result<String, String> {
    let value: CfStringRef = property_data(object, selector, SCOPE_GLOBAL)?;
    let owned = CfOwned::new(value, "device UID")?;
    let mut buffer = [0_i8; 1024];
    let converted = unsafe {
        CFStringGetCString(
            owned.0,
            buffer.as_mut_ptr(),
            buffer.len() as CfIndex,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if !converted {
        return Err("CoreAudio device identifier could not be decoded".into());
    }
    unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "CoreAudio device identifier is not UTF-8".into())
}

fn object_for_uid(uid: &str) -> Result<AudioObjectId, String> {
    let mut found = None;
    for device in device_ids()? {
        if string_property(device, PROP_DEVICE_UID)? == uid {
            if found.replace(device).is_some() {
                return Err("CoreAudio returned an ambiguous device identifier".into());
            }
        }
    }
    found.ok_or_else(|| "Requested CoreAudio device is unavailable".into())
}

fn default_output_uid() -> Result<String, String> {
    let device: AudioObjectId =
        property_data(AUDIO_OBJECT_SYSTEM, PROP_DEFAULT_OUTPUT, SCOPE_GLOBAL)?;
    if device == AUDIO_OBJECT_UNKNOWN {
        return Err("CoreAudio has no default output device".into());
    }
    string_property(device, PROP_DEVICE_UID)
}

fn physical_sample_rate(uid: &str) -> Result<f64, String> {
    let object = object_for_uid(uid)?;
    let rate: f64 = property_data(object, PROP_NOMINAL_RATE, SCOPE_GLOBAL)?;
    if !rate.is_finite() || rate <= 0.0 {
        return Err("CoreAudio returned an invalid physical sample rate".into());
    }
    Ok(rate)
}

fn output_channel_count(object: AudioObjectId) -> Result<u32, String> {
    let address = property_address(PROP_STREAM_CONFIGURATION, SCOPE_OUTPUT);
    let mut byte_size = 0;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut byte_size) };
    if status != 0 || byte_size < std::mem::size_of::<u32>() as u32 {
        return Err(format!(
            "CoreAudio output layout query failed (status {status})"
        ));
    }
    // AudioBufferList has a variable-length trailing array. u64 storage gives
    // the allocation the alignment required by AudioBuffer on arm64.
    let words = (byte_size as usize).div_ceil(std::mem::size_of::<u64>());
    let mut storage = vec![0_u64; words];
    let mut returned_size = byte_size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut returned_size,
            storage.as_mut_ptr().cast(),
        )
    };
    if status != 0 || returned_size as usize > storage.len() * std::mem::size_of::<u64>() {
        return Err(format!(
            "CoreAudio output layout query failed (status {status})"
        ));
    }
    let data = storage.as_ptr().cast::<u8>();
    let buffers_offset = std::mem::offset_of!(AudioBufferList, buffers);
    if (returned_size as usize) < buffers_offset {
        return Err("CoreAudio returned a truncated output layout".into());
    }
    let count = unsafe { ptr::read_unaligned(data.cast::<u32>()) } as usize;
    let buffers_size = count
        .checked_mul(std::mem::size_of::<AudioBuffer>())
        .and_then(|size| buffers_offset.checked_add(size))
        .ok_or_else(|| "CoreAudio output layout size overflowed".to_owned())?;
    if buffers_size > returned_size as usize {
        return Err("CoreAudio returned a truncated output layout".into());
    }
    let mut channels = 0_u32;
    for index in 0..count {
        let offset = buffers_offset + index * std::mem::size_of::<AudioBuffer>();
        let buffer = unsafe { ptr::read_unaligned(data.add(offset).cast::<AudioBuffer>()) };
        channels = channels
            .checked_add(buffer.channels)
            .ok_or_else(|| "CoreAudio channel count overflowed".to_owned())?;
    }
    Ok(channels)
}

fn physical_device(uid: &str) -> Result<(AudioObjectId, f64), String> {
    let object = object_for_uid(uid)?;
    if output_channel_count(object)? != 2 {
        return Err("Each aggregate source must have exactly two output channels".into());
    }
    let rate: f64 = property_data(object, PROP_NOMINAL_RATE, SCOPE_GLOBAL)?;
    if !rate.is_finite() || rate <= 0.0 {
        return Err("CoreAudio returned an invalid physical sample rate".into());
    }
    Ok((object, rate))
}

pub(crate) fn physical_output_channels(uid: &str) -> Result<u32, String> {
    output_channel_count(object_for_uid(uid)?)
}

fn device_uid_is_present(uid: &str) -> Result<bool, String> {
    for device in device_ids()? {
        if string_property(device, PROP_DEVICE_UID)? == uid {
            return Ok(true);
        }
    }
    Ok(false)
}

fn device_description(uid: &str, drift: bool) -> Result<CfOwned, String> {
    let uid = cf_string(uid)?;
    let mut entries = vec![("uid", uid)];
    if drift {
        entries.push(("drift", cf_number(1)?));
    }
    cf_dictionary(&entries)
}

/// A process-private aggregate of two existing, compatible stereo outputs.
///
/// The guard owns only the AudioObjectID returned by its own successful
/// `AudioHardwareCreateAggregateDevice` call. It never changes defaults or
/// physical device formats, and never removes any other aggregate.
pub(crate) struct PrivateAggregateGuard {
    object_id: AudioObjectId,
    uid: String,
    name: String,
    source_uids: [String; 2],
    source_rates: [f64; 2],
    original_default_uid: String,
    stream_teardown: Vec<Arc<AtomicBool>>,
    retain_for_process_exit: bool,
    destroyed: bool,
}

impl PrivateAggregateGuard {
    /// Create a private, stacked two-device aggregate using exact device UIDs.
    pub(crate) fn create(first_uid: &str, second_uid: &str) -> Result<Self, String> {
        if first_uid == second_uid {
            return Err("Aggregate sources must be distinct devices".into());
        }
        let original_default_uid = default_output_uid()?;
        let (first_object, first_rate) = physical_device(first_uid)?;
        let (second_object, second_rate) = physical_device(second_uid)?;
        if first_object == second_object || first_rate.to_bits() != second_rate.to_bits() {
            return Err(
                "Aggregate sources must be distinct and already share a sample rate".into(),
            );
        }

        let uuid = unique_uuid()?;
        let uid = format!("com.portalsurfer.yield.native-test.{uuid}");
        let name = format!("Yield native test {uuid}");
        let subdevices = [
            device_description(first_uid, false)?,
            device_description(second_uid, true)?,
        ];
        let subdevices = cf_array(&subdevices)?;
        let description = cf_dictionary(&[
            ("uid", cf_string(&uid)?),
            ("name", cf_string(&name)?),
            ("private", cf_number(1)?),
            ("stacked", cf_number(1)?),
            ("master", cf_string(first_uid)?),
            ("subdevices", subdevices),
        ])?;

        let mut object_id = AUDIO_OBJECT_UNKNOWN;
        let status = unsafe { AudioHardwareCreateAggregateDevice(description.0, &mut object_id) };
        if status != 0 {
            return Err(format!(
                "CoreAudio aggregate creation failed (status {status})"
            ));
        }

        // Establish ownership immediately after the successful create call.
        Ok(Self {
            object_id,
            uid,
            name,
            source_uids: [first_uid.to_owned(), second_uid.to_owned()],
            source_rates: [first_rate, second_rate],
            original_default_uid,
            stream_teardown: Vec::new(),
            retain_for_process_exit: false,
            destroyed: false,
        })
    }

    /// Bind a native stream's asynchronous teardown to this aggregate's
    /// lifetime. Every attached stream must be destroyed before the aggregate.
    pub(crate) fn track_stream_teardown(&mut self, complete: Arc<AtomicBool>) {
        self.stream_teardown.push(complete);
    }

    /// Resolve the exact hidden CoreAudio object created by this guard.
    ///
    /// CPAL's normal device enumeration intentionally omits process-private
    /// aggregate devices. Its CoreAudio backend provides `Device::new` for
    /// hidden devices, so bind through the AudioObjectID returned by our
    /// successful create call and then verify the generated UID.
    pub(crate) fn cpal_device(&self) -> Result<cpal::Device, String> {
        use cpal::traits::DeviceTrait;

        let coreaudio_device =
            <<cpal::platform::CoreAudioHost as cpal::traits::HostTrait>::Device>::new(
                self.object_id,
            );
        let device: cpal::Device = cpal::platform::DeviceInner::CoreAudio(coreaudio_device).into();
        let device_id = device.id().map_err(|error| error.to_string())?;
        if device_id.0 != cpal::HostId::CoreAudio || device_id.1 != self.uid {
            return Err("CPAL direct-ID binding did not preserve the owned aggregate UID".into());
        }
        let description = device.description().map_err(|error| error.to_string())?;
        if description.name() != self.name {
            return Err("CPAL direct-ID binding did not preserve the owned aggregate name".into());
        }
        Ok(device)
    }

    /// Keep the fixture registered until process exit when stream startup
    /// failed before it could return a teardown observer to the test.
    pub(crate) fn retain_for_process_exit(&mut self) {
        self.retain_for_process_exit = true;
    }

    /// The generated name is safe to display in test diagnostics.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The generated UID identifies only this guard's private aggregate.
    pub(crate) fn uid(&self) -> &str {
        &self.uid
    }

    /// Destroy this guard's device and verify it disappears without changing
    /// the system default or either source's physical sample rate.
    pub(crate) fn close(mut self) -> Result<(), String> {
        let result = self.destroy_and_verify();
        result
    }

    fn destroy_and_verify(&mut self) -> Result<(), String> {
        if !self.wait_for_stream_teardown() {
            self.retain_for_process_exit = true;
            return Err(
                "native stream teardown did not complete; retaining the private aggregate for process cleanup".into(),
            );
        }

        if !self.destroyed {
            let status = unsafe { AudioHardwareDestroyAggregateDevice(self.object_id) };
            if status != 0 {
                return Err(format!(
                    "CoreAudio aggregate destruction failed (status {status})"
                ));
            }
            self.destroyed = true;
        }

        let deadline = Instant::now() + DESTROY_WAIT;
        loop {
            if !device_uid_is_present(&self.uid)? {
                break;
            }
            if Instant::now() >= deadline {
                return Err(
                    "CoreAudio aggregate did not disappear before the cleanup deadline".into(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }

        if default_output_uid()? != self.original_default_uid {
            return Err("CoreAudio default output changed while the fixture was active".into());
        }
        for (uid, expected_rate) in self.source_uids.iter().zip(self.source_rates) {
            if physical_sample_rate(uid)?.to_bits() != expected_rate.to_bits() {
                return Err(
                    "A CoreAudio source sample rate changed while the fixture was active".into(),
                );
            }
        }
        Ok(())
    }

    fn wait_for_stream_teardown(&self) -> bool {
        let deadline = Instant::now() + DESTROY_WAIT;
        loop {
            if self
                .stream_teardown
                .iter()
                .all(|complete| complete.load(Ordering::Acquire))
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for PrivateAggregateGuard {
    fn drop(&mut self) {
        if self.destroyed || self.retain_for_process_exit {
            return;
        }
        // Unwinding follows the same ownership rule as the explicit close
        // path. If a stream is stuck, leave only this private aggregate for
        // CoreAudio/process cleanup instead of destroying a live device.
        let _ = self.destroy_and_verify();
    }
}
