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
const PROP_OBJECT_CLASS: u32 = fourcc(b"clas");
const PROP_DEVICE_TRANSPORT: u32 = fourcc(b"tran");
const CLASS_DEVICE: u32 = fourcc(b"adev");
const CLASS_AGGREGATE_DEVICE: u32 = fourcc(b"aagg");
const TRANSPORT_VIRTUAL: u32 = fourcc(b"virt");
const TRANSPORT_AGGREGATE: u32 = fourcc(b"grup");
const PROP_NOMINAL_RATE: u32 = fourcc(b"nsrt");
const PROP_STREAM_CONFIGURATION: u32 = fourcc(b"slay");
const PROP_AGGREGATE_FULL_SUBDEVICE_LIST: u32 = fourcc(b"grup");
const PROP_AGGREGATE_ACTIVE_SUBDEVICE_LIST: u32 = fourcc(b"agrp");
const PROP_AGGREGATE_COMPOSITION: u32 = fourcc(b"acom");
const AUDIO_HARDWARE_BAD_OBJECT_ERROR: i32 = fourcc(b"!obj") as i32;
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
    fn CFNumberGetValue(number: CfTypeRef, number_type: CfIndex, value: *mut c_void) -> bool;
    fn CFArrayGetCount(array: CfTypeRef) -> CfIndex;
    fn CFArrayGetValueAtIndex(array: CfTypeRef, index: CfIndex) -> CfTypeRef;
    fn CFDictionaryGetValue(dictionary: CfTypeRef, key: CfTypeRef) -> CfTypeRef;
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

fn borrowed_cf_string(value: CfStringRef) -> Result<String, String> {
    if value.is_null() {
        return Err("CoreAudio returned a null string value".into());
    }
    let mut buffer = [0_i8; 1024];
    let converted = unsafe {
        CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            buffer.len() as CfIndex,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if !converted {
        return Err("CoreAudio string value could not be decoded as UTF-8".into());
    }
    unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .map(str::to_owned)
        .map_err(|_| "CoreAudio string value is not UTF-8".into())
}

fn dictionary_value(dictionary: CfTypeRef, key: &str) -> Result<CfTypeRef, String> {
    let key_name = key;
    let key = cf_string(key)?;
    let value = unsafe { CFDictionaryGetValue(dictionary, key.0) };
    if value.is_null() {
        Err(format!("CoreAudio composition is missing {key_name:?}"))
    } else {
        Ok(value)
    }
}

fn dictionary_string(dictionary: CfTypeRef, key: &str) -> Result<String, String> {
    borrowed_cf_string(dictionary_value(dictionary, key)?)
}

fn dictionary_i32(dictionary: CfTypeRef, key: &str) -> Result<i32, String> {
    let number = dictionary_value(dictionary, key)?;
    let mut value = 0_i32;
    if unsafe { CFNumberGetValue(number, CF_NUMBER_SINT32, (&mut value as *mut i32).cast()) } {
        Ok(value)
    } else {
        Err(format!("CoreAudio composition value {key:?} is not an i32"))
    }
}

fn array_strings(array: CfTypeRef) -> Result<Vec<String>, String> {
    if array.is_null() {
        return Err("CoreAudio returned a null array".into());
    }
    let count = unsafe { CFArrayGetCount(array) };
    if count < 0 || count > 64 {
        return Err(format!(
            "CoreAudio returned an unexpected array length {count}"
        ));
    }
    (0..count)
        .map(|index| borrowed_cf_string(unsafe { CFArrayGetValueAtIndex(array, index) }))
        .collect()
}

fn audio_object_id_array(
    object: AudioObjectId,
    selector: u32,
) -> Result<Vec<AudioObjectId>, String> {
    let address = property_address(selector, SCOPE_GLOBAL);
    let mut byte_size = 0;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut byte_size) };
    if status != 0 || byte_size as usize % std::mem::size_of::<AudioObjectId>() != 0 {
        return Err(format!(
            "CoreAudio object-ID array query failed (status {status})"
        ));
    }
    let mut values = vec![0; byte_size as usize / std::mem::size_of::<AudioObjectId>()];
    let mut returned_size = byte_size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut returned_size,
            values.as_mut_ptr().cast(),
        )
    };
    if status != 0
        || returned_size as usize > byte_size as usize
        || returned_size as usize % std::mem::size_of::<AudioObjectId>() != 0
    {
        return Err(format!(
            "CoreAudio object-ID array query failed (status {status})"
        ));
    }
    values.truncate(returned_size as usize / std::mem::size_of::<AudioObjectId>());
    Ok(values)
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

fn fourcc_text(value: u32) -> String {
    let bytes = value.to_be_bytes();
    if bytes.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        format!("{} (0x{value:08x})", String::from_utf8_lossy(&bytes))
    } else {
        format!("0x{value:08x}")
    }
}

fn aggregate_full_uid_order(object: AudioObjectId) -> Result<Vec<String>, String> {
    let value: CfTypeRef = property_data(object, PROP_AGGREGATE_FULL_SUBDEVICE_LIST, SCOPE_GLOBAL)?;
    let value = CfOwned::new(value, "aggregate full subdevice UID list")?;
    array_strings(value.0)
}

fn aggregate_active_devices(object: AudioObjectId) -> Result<Vec<AudioObjectId>, String> {
    audio_object_id_array(object, PROP_AGGREGATE_ACTIVE_SUBDEVICE_LIST)
}

fn aggregate_composition(object: AudioObjectId) -> Result<(i32, i32, String, Vec<String>), String> {
    let value: CfTypeRef = property_data(object, PROP_AGGREGATE_COMPOSITION, SCOPE_GLOBAL)?;
    let composition = CfOwned::new(value, "aggregate composition")?;
    let private = dictionary_i32(composition.0, "private")?;
    let stacked = dictionary_i32(composition.0, "stacked")?;
    let master = dictionary_string(composition.0, "master")?;
    let subdevices = dictionary_value(composition.0, "subdevices")?;
    let count = unsafe { CFArrayGetCount(subdevices) };
    if !(0..=64).contains(&count) {
        return Err(format!(
            "aggregate composition has unexpected subdevice count {count}"
        ));
    }
    let subdevices = (0..count)
        .map(|index| {
            let dictionary = unsafe { CFArrayGetValueAtIndex(subdevices, index) };
            let uid = dictionary_string(dictionary, "uid")?;
            Ok(uid)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((private, stacked, master, subdevices))
}

fn physical_device(uid: &str) -> Result<(AudioObjectId, f64), String> {
    let object = object_for_uid(uid)?;
    validate_physical_device(object)?;
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
    let object = object_for_uid(uid)?;
    validate_physical_device(object)?;
    output_channel_count(object)
}

fn validate_physical_device(object: AudioObjectId) -> Result<(), String> {
    let class: u32 = property_data(object, PROP_OBJECT_CLASS, SCOPE_GLOBAL)?;
    if class != CLASS_DEVICE {
        return Err("Aggregate source is not a physical CoreAudio device".into());
    }
    let transport: u32 = property_data(object, PROP_DEVICE_TRANSPORT, SCOPE_GLOBAL)?;
    if matches!(transport, TRANSPORT_VIRTUAL | TRANSPORT_AGGREGATE) {
        return Err("Aggregate source has a virtual or aggregate transport".into());
    }
    Ok(())
}

fn owned_device_uid_is_present(object: AudioObjectId, expected_uid: &str) -> Result<bool, String> {
    let address = property_address(PROP_DEVICE_UID, SCOPE_GLOBAL);
    let mut value = std::mem::MaybeUninit::<CfStringRef>::uninit();
    let mut size = std::mem::size_of::<CfStringRef>() as u32;
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
    if status == AUDIO_HARDWARE_BAD_OBJECT_ERROR {
        return Ok(false);
    }
    if status != 0 || size as usize != std::mem::size_of::<CfStringRef>() {
        return Err(format!(
            "owned CoreAudio aggregate identity query failed (status {status})"
        ));
    }
    let uid = CfOwned::new(unsafe { value.assume_init() }, "owned aggregate UID")?;
    Ok(borrowed_cf_string(uid.0)? == expected_uid)
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
        // On the qualified host, stacked=1 mirrored these stereo devices as two channels;
        // stacked=0 exposed four separately addressable output channels.
        let description = cf_dictionary(&[
            ("uid", cf_string(&uid)?),
            ("name", cf_string(&name)?),
            ("private", cf_number(1)?),
            ("stacked", cf_number(0)?),
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
        let guard = Self {
            object_id,
            uid,
            name,
            source_uids: [first_uid.to_owned(), second_uid.to_owned()],
            source_rates: [first_rate, second_rate],
            original_default_uid,
            stream_teardown: Vec::new(),
            retain_for_process_exit: false,
            destroyed: false,
        };
        if let Err(readiness_error) = guard.wait_until_ready() {
            return match guard.close() {
                Ok(()) => Err(readiness_error),
                Err(cleanup_error) => Err(format!(
                    "{readiness_error}; owned aggregate cleanup also failed: {cleanup_error}"
                )),
            };
        }
        Ok(guard)
    }

    fn wait_until_ready(&self) -> Result<(), String> {
        let deadline = Instant::now() + DESTROY_WAIT;
        loop {
            match self.validate_topology() {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() >= deadline => {
                    return Err(format!(
                        "private aggregate {} did not become ready: {}; {}",
                        self.uid,
                        error,
                        self.topology_diagnostic()
                    ));
                }
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn validate_topology(&self) -> Result<(), String> {
        if string_property(self.object_id, PROP_DEVICE_UID)? != self.uid {
            return Err("owned aggregate UID does not match the requested UID".into());
        }
        let class: u32 = property_data(self.object_id, PROP_OBJECT_CLASS, SCOPE_GLOBAL)?;
        if class != CLASS_AGGREGATE_DEVICE {
            return Err(format!("owned object class is {}", fourcc_text(class)));
        }
        let rate: f64 = property_data(self.object_id, PROP_NOMINAL_RATE, SCOPE_GLOBAL)?;
        if rate.to_bits() != self.source_rates[0].to_bits() {
            return Err(format!(
                "aggregate nominal rate is {rate:.3}, expected {:.3}",
                self.source_rates[0]
            ));
        }
        let channels = output_channel_count(self.object_id)?;
        if channels != 4 {
            return Err(format!(
                "aggregate exposes {channels} output channels, expected 4"
            ));
        }
        let expected_uids = self.source_uids.to_vec();
        if aggregate_full_uid_order(self.object_id)? != expected_uids {
            return Err(
                "aggregate full subdevice UID order differs from the selected children".into(),
            );
        }
        let expected_ids = self
            .source_uids
            .iter()
            .map(|uid| object_for_uid(uid))
            .collect::<Result<Vec<_>, _>>()?;
        if aggregate_active_devices(self.object_id)? != expected_ids {
            return Err("aggregate active subdevices differ from the selected children".into());
        }
        let (private, stacked, master, subdevices) = aggregate_composition(self.object_id)?;
        if private != 1
            || stacked != 0
            || master != self.source_uids[0]
            || subdevices != expected_uids
        {
            return Err(format!(
                "aggregate composition differs (private={private}, stacked={stacked}, master={master:?}, subdevices={subdevices:?})"
            ));
        }
        Ok(())
    }

    fn topology_diagnostic(&self) -> String {
        let class = property_data::<u32>(self.object_id, PROP_OBJECT_CLASS, SCOPE_GLOBAL)
            .map(fourcc_text)
            .unwrap_or_else(|error| error);
        let channels = output_channel_count(self.object_id)
            .map(|channels| channels.to_string())
            .unwrap_or_else(|error| error);
        let rate = property_data::<f64>(self.object_id, PROP_NOMINAL_RATE, SCOPE_GLOBAL)
            .map(|rate| format!("{rate:.3}"))
            .unwrap_or_else(|error| error);
        let composition = aggregate_composition(self.object_id)
            .map(|(private, stacked, master, subdevices)| {
                format!(
                    "private={private}, stacked={stacked}, master={master:?}, subdevices={subdevices:?}"
                )
            })
            .unwrap_or_else(|error| error);
        format!("class={class}, channels={channels}, rate={rate}, {composition}")
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
            if !owned_device_uid_is_present(self.object_id, &self.uid)? {
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
