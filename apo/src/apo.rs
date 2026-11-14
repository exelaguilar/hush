//! COM layer for the APO. Mirrors the contract of Microsoft's
//! `CBaseAudioProcessingObject`:
//!
//! * `Initialize` once, then `LockForProcess` fixes the format and allocates;
//! * `APOProcess` runs on the engine's real-time thread only between
//!   `LockForProcess` and `UnlockForProcess` and must not block or allocate;
//! * the engine never calls configuration methods concurrently with
//!   `APOProcess`, which is what makes the unsynchronised state below sound.
//!
//! **Aggregation.** Windows' audio engine creates system-effect APOs as
//! *aggregated* COM objects (it passes an outer `IUnknown` to
//! `CreateInstance`). `windows-rs`'s `#[implement]` cannot be aggregated, so
//! the object here is written by hand with the standard pattern: a private
//! non-delegating `IUnknown` that owns the reference count, and public
//! interfaces whose `IUnknown` methods forward to the controlling unknown
//! (the engine's outer object, or our own inner unknown when not aggregated).

use crate::{
    dsp::{CleanMic, MicSettings},
    settings, COPYRIGHT, FRIENDLY_NAME,
};
use std::{
    cell::UnsafeCell,
    ffi::c_void,
    mem::offset_of,
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};
use windows::Win32::{
    Foundation::{
        CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOINTERFACE, E_OUTOFMEMORY, E_POINTER,
        S_FALSE, S_OK,
    },
    Media::Audio::Apo::{
        IAudioMediaType, IAudioProcessingObject, IAudioProcessingObjectConfiguration,
        IAudioProcessingObjectConfiguration_Vtbl, IAudioProcessingObjectRT,
        IAudioProcessingObjectRT_Vtbl, IAudioProcessingObject_Vtbl, IAudioSystemEffects,
        IAudioSystemEffects_Vtbl, APOERR_ALREADY_INITIALIZED, APOERR_ALREADY_UNLOCKED,
        APOERR_APO_LOCKED, APOERR_FORMAT_NOT_SUPPORTED, APOERR_INVALID_CONNECTION_FORMAT,
        APOERR_NOT_INITIALIZED, APOERR_NUM_CONNECTIONS_INVALID, APO_CONNECTION_DESCRIPTOR,
        APO_CONNECTION_PROPERTY, APO_FLAG, APO_FLAG_DEFAULT, APO_FLAG_INPLACE, APO_REG_PROPERTIES,
        BUFFER_VALID, UNCOMPRESSEDAUDIOFORMAT,
    },
    System::Com::{CoTaskMemAlloc, IClassFactory, IClassFactory_Impl},
};
use windows_core::{
    implement, IUnknown, IUnknown_Vtbl, Interface, Ref, Result, BOOL, GUID, HRESULT,
};

pub const CLSID: GUID = GUID::from_u128(0x8AA11902_CC87_49EB_B601_AA1449A395A6);

const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

// ---------------------------------------------------------------------------
// Processing logic (independent of the COM object layout)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct State {
    initialized: bool,
    locked: bool,
    channels: usize,
    rate: u32,
    max_frames: u32,
    dsp: Option<Box<CleanMic>>,
    settings: MicSettings,
    input_backup: Vec<f32>,
    /// The DSP panicked during this stream: pass audio through unchanged
    /// until the next LockForProcess.
    faulted: bool,
    #[cfg(test)]
    inject_processing_fault: bool,
}

struct Core {
    state: UnsafeCell<State>,
}

impl Core {
    #[allow(clippy::mut_from_ref)]
    fn state(&self) -> &mut State {
        // SAFETY: the engine serialises calls on one APO instance (see the
        // module docs); the state is never accessed from two threads at once.
        unsafe { &mut *self.state.get() }
    }
}

fn read_format(media_type: &IAudioMediaType) -> Option<UNCOMPRESSEDAUDIOFORMAT> {
    let mut format = UNCOMPRESSEDAUDIOFORMAT::default();
    unsafe { media_type.GetUncompressedAudioFormat(&mut format) }.ok()?;
    Some(format)
}

/// The engine runs APO chains in 32-bit float; that is all we accept.
fn is_supported(format: &UNCOMPRESSEDAUDIOFORMAT) -> bool {
    format.guidFormatType == SUBTYPE_IEEE_FLOAT
        && format.dwBytesPerSampleContainer == 4
        && format.dwValidBitsPerSample == 32
        && (1..=8).contains(&format.dwSamplesPerFrame)
        && (8_000.0..=384_000.0).contains(&format.fFramesPerSecond)
}

fn describe(format: Option<&UNCOMPRESSEDAUDIOFORMAT>) -> String {
    match format {
        None => "none".into(),
        Some(f) => format!(
            "{} {}ch {}bit/{}B {}Hz mask {:#x}",
            if f.guidFormatType == SUBTYPE_IEEE_FLOAT {
                "float".to_string()
            } else {
                format!("{:?}", f.guidFormatType)
            },
            f.dwSamplesPerFrame,
            f.dwValidBitsPerSample,
            f.dwBytesPerSampleContainer,
            f.fFramesPerSecond,
            f.dwChannelMask
        ),
    }
}

fn check_format(
    opposite: Option<&IAudioMediaType>,
    requested: Option<&IAudioMediaType>,
) -> Result<IAudioMediaType> {
    let requested = requested.ok_or(E_POINTER)?;
    let format = read_format(requested).ok_or(APOERR_FORMAT_NOT_SUPPORTED)?;
    if !is_supported(&format) {
        return Err(APOERR_FORMAT_NOT_SUPPORTED.into());
    }
    // Format-preserving APO: input and output must match.
    if let Some(other) = opposite.and_then(read_format) {
        if other.dwSamplesPerFrame != format.dwSamplesPerFrame
            || other.fFramesPerSecond != format.fFramesPerSecond
        {
            return Err(APOERR_FORMAT_NOT_SUPPORTED.into());
        }
    }
    Ok(requested.clone())
}

fn copy_wide(destination: &mut [u16], text: &str) {
    // Leave the last slot as the NUL terminator.
    let limit = destination.len().saturating_sub(1);
    for (slot, unit) in destination[..limit].iter_mut().zip(text.encode_utf16()) {
        *slot = unit;
    }
}

pub fn registration_properties() -> APO_REG_PROPERTIES {
    // SAFETY: plain-old-data struct; all-zero is a valid (empty) value.
    let mut properties: APO_REG_PROPERTIES = unsafe { std::mem::zeroed() };
    properties.clsid = CLSID;
    // DEFAULT | INPLACE (15): what every system effect on this PC declares;
    // APOProcess handles shared input/output buffers.
    properties.Flags = APO_FLAG(APO_FLAG_DEFAULT.0 | APO_FLAG_INPLACE.0);
    copy_wide(&mut properties.szFriendlyName, FRIENDLY_NAME);
    copy_wide(&mut properties.szCopyrightInfo, COPYRIGHT);
    properties.u32MajorVersion = 1;
    properties.u32MinorVersion = 0;
    properties.u32MinInputConnections = 1;
    properties.u32MaxInputConnections = 1;
    properties.u32MinOutputConnections = 1;
    properties.u32MaxOutputConnections = 1;
    properties.u32MaxInstances = u32::MAX;
    properties.u32NumAPOInterfaces = 1;
    properties.iidAPOInterfaceList[0] = IAudioProcessingObject::IID;
    properties
}

/// Sets flush-to-zero / denormals-are-zero for the duration of a process
/// call (denormals make IIR filters and RNNoise very slow near silence) and
/// restores the engine's MXCSR afterwards.
#[cfg(target_arch = "x86_64")]
struct FlushDenormals(u32);

#[cfg(target_arch = "x86_64")]
impl FlushDenormals {
    #[allow(deprecated)]
    fn enable() -> Self {
        use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
        unsafe {
            let previous = _mm_getcsr();
            _mm_setcsr(previous | 0x8040);
            Self(previous)
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl Drop for FlushDenormals {
    #[allow(deprecated)]
    fn drop(&mut self) {
        unsafe { std::arch::x86_64::_mm_setcsr(self.0) };
    }
}

impl Core {
    fn reset(&self) -> Result<()> {
        if let Some(dsp) = self.state().dsp.as_mut() {
            dsp.reset();
        }
        Ok(())
    }

    fn latency(&self) -> i64 {
        let rate = match self.state().rate {
            0 => crate::denoise::RATE,
            rate => rate,
        };
        // 100-ns units.
        CleanMic::latency_frames(rate) as i64 * 10_000_000 / i64::from(rate)
    }

    fn initialize(&self, size: u32, data: *const u8) -> Result<()> {
        let state = self.state();
        let header = if data.is_null() || (size as usize) < 4 {
            "no data".to_string()
        } else {
            // APOInitBaseStruct starts with cbSize: identifies which
            // APOInitSystemEffects version the engine passed.
            let cb = unsafe { (data as *const u32).read_unaligned() };
            format!("struct cbSize {cb}")
        };
        settings::trace(&format!(
            "Initialize: {size} bytes, {header}; locked {} initialized {}",
            state.locked, state.initialized
        ));
        if state.locked {
            return Err(APOERR_APO_LOCKED.into());
        }
        if state.initialized {
            return Err(APOERR_ALREADY_INITIALIZED.into());
        }
        state.initialized = true;
        Ok(())
    }

    fn format_supported(
        &self,
        which: &str,
        opposite: Option<&IAudioMediaType>,
        requested: Option<&IAudioMediaType>,
    ) -> Result<IAudioMediaType> {
        let result = check_format(opposite, requested);
        settings::trace(&format!(
            "{which}: requested [{}] opposite [{}] -> {}",
            describe(requested.and_then(read_format).as_ref()),
            describe(opposite.and_then(read_format).as_ref()),
            match &result {
                Ok(_) => "accepted".to_string(),
                Err(e) => format!("rejected {:?}", e.code()),
            }
        ));
        result
    }

    fn input_channel_count(&self) -> Result<u32> {
        let state = self.state();
        settings::trace(&format!(
            "GetInputChannelCount: locked {} channels {}",
            state.locked, state.channels
        ));
        if !state.locked {
            return Err(APOERR_NOT_INITIALIZED.into());
        }
        Ok(state.channels as u32)
    }

    fn lock(
        &self,
        input_count: u32,
        inputs: *const *const APO_CONNECTION_DESCRIPTOR,
        output_count: u32,
        outputs: *const *const APO_CONNECTION_DESCRIPTOR,
    ) -> Result<()> {
        let state = self.state();
        if !state.initialized {
            return Err(APOERR_NOT_INITIALIZED.into());
        }
        if state.locked {
            return Err(APOERR_APO_LOCKED.into());
        }
        if input_count != 1 || output_count != 1 || inputs.is_null() || outputs.is_null() {
            return Err(APOERR_NUM_CONNECTIONS_INVALID.into());
        }
        let (input, output) = unsafe {
            let input = *inputs;
            let output = *outputs;
            if input.is_null() || output.is_null() {
                return Err(APOERR_NUM_CONNECTIONS_INVALID.into());
            }
            (&*input, &*output)
        };
        let input_format = (*input.pFormat)
            .as_ref()
            .and_then(read_format)
            .ok_or(APOERR_INVALID_CONNECTION_FORMAT)?;
        let output_format = (*output.pFormat)
            .as_ref()
            .and_then(read_format)
            .ok_or(APOERR_INVALID_CONNECTION_FORMAT)?;
        settings::trace(&format!(
            "LockForProcess formats: in [{}] out [{}]",
            describe(Some(&input_format)),
            describe(Some(&output_format))
        ));
        if !is_supported(&input_format)
            || !is_supported(&output_format)
            || input_format.dwSamplesPerFrame != output_format.dwSamplesPerFrame
            || input_format.fFramesPerSecond != output_format.fFramesPerSecond
        {
            return Err(APOERR_INVALID_CONNECTION_FORMAT.into());
        }

        state.channels = input_format.dwSamplesPerFrame as usize;
        state.rate = input_format.fFramesPerSecond as u32;
        state.max_frames = input.u32MaxFrameCount.min(output.u32MaxFrameCount);
        // All allocation for the stream happens here, off the real-time thread.
        state.input_backup = vec![0.0; state.max_frames as usize * state.channels];
        state.dsp = Some(Box::new(CleanMic::new(state.rate, state.channels)));
        state.settings = settings::current().unwrap_or_default();
        state.faulted = false;
        state.locked = true;
        Ok(())
    }

    fn unlock(&self) -> Result<()> {
        settings::trace("UnlockForProcess");
        let state = self.state();
        if !state.locked {
            return Err(APOERR_ALREADY_UNLOCKED.into());
        }
        state.locked = false;
        state.dsp = None;
        state.input_backup = Vec::new();
        Ok(())
    }

    /// Real-time thread: no allocation, locks, logging or I/O.
    fn process(
        &self,
        input_count: u32,
        inputs: *const *const APO_CONNECTION_PROPERTY,
        output_count: u32,
        outputs: *mut *mut APO_CONNECTION_PROPERTY,
    ) {
        let state = self.state();
        let Some(dsp) = state.dsp.as_mut() else {
            return;
        };
        if !state.locked
            || input_count != 1
            || output_count != 1
            || inputs.is_null()
            || outputs.is_null()
        {
            return;
        }
        let (input, output) = unsafe {
            let input = *inputs;
            let output = *outputs;
            if input.is_null() || output.is_null() {
                return;
            }
            (*input, &mut *output)
        };
        let frames = input.u32ValidFrameCount.min(state.max_frames);
        let samples = frames as usize * state.channels;
        let source = input.pBuffer as *const f32;
        let destination = output.pBuffer as *mut f32;
        if destination.is_null() {
            return;
        }

        #[cfg(target_arch = "x86_64")]
        let _ftz = FlushDenormals::enable();

        unsafe {
            let valid = !source.is_null() && input.u32BufferFlags == BUFFER_VALID;
            let in_place = valid && std::ptr::eq(source, destination);
            if !valid {
                // BUFFER_SILENT (or invalid): contents are undefined, use silence.
                ptr::write_bytes(destination, 0, samples);
            } else if !in_place {
                ptr::copy_nonoverlapping(source, destination, samples);
            } else if !state.faulted {
                ptr::copy_nonoverlapping(source, state.input_backup.as_mut_ptr(), samples);
            }
            // A panic must not unwind out of this extern "system" callback:
            // that aborts audiodg.exe and takes down all Windows audio.
            // Catching costs nothing unless a panic happens.
            if !state.faulted {
                let buffer = std::slice::from_raw_parts_mut(destination, samples);
                if let Some(settings) = settings::current() {
                    state.settings = settings;
                }
                #[cfg(test)]
                let inject_fault = state.inject_processing_fault;
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if inject_fault {
                        buffer[0] = 0.0;
                        panic!("test fault after an in-place write");
                    }
                    dsp.process(buffer, &state.settings)
                }))
                .is_err()
                {
                    state.faulted = true;
                    settings::report_fault();
                    if !valid {
                        buffer.fill(0.0);
                    } else {
                        let original = if in_place {
                            state.input_backup.as_ptr()
                        } else {
                            source
                        };
                        ptr::copy_nonoverlapping(original, buffer.as_mut_ptr(), samples);
                    }
                }
            }
        }
        settings::record_processed(frames, state.rate, state.channels as u32);
        output.u32ValidFrameCount = frames;
        // Even after silent input the delayed tail may still carry audio.
        output.u32BufferFlags = BUFFER_VALID;
    }
}

#[cfg(test)]
mod fault_tests {
    use super::*;

    #[test]
    fn partial_in_place_write_is_restored_on_fault() {
        let core = Core {
            state: UnsafeCell::new(State {
                locked: true,
                channels: 1,
                rate: 48_000,
                max_frames: 2,
                dsp: Some(Box::new(CleanMic::new_inline(48_000, 1))),
                input_backup: vec![0.0; 2],
                inject_processing_fault: true,
                ..State::default()
            }),
        };
        let mut samples = [0.5_f32, -0.25];
        let mut connection = APO_CONNECTION_PROPERTY {
            pBuffer: samples.as_mut_ptr() as usize,
            u32BufferFlags: BUFFER_VALID,
            u32ValidFrameCount: 2,
            ..Default::default()
        };
        let mut output = &raw mut connection;
        let input = output.cast_const();
        core.process(1, &input, 1, &mut output);
        assert!(core.state().faulted);
        assert_eq!(samples, [0.5, -0.25]);
        samples.copy_from_slice(&[0.375, 0.25]);
        core.process(1, &input, 1, &mut output);
        assert_eq!(samples, [0.375, 0.25]);
    }
}

// ---------------------------------------------------------------------------
// Hand-written, aggregatable COM object
// ---------------------------------------------------------------------------

#[repr(C)]
struct ApoObject {
    apo: *const IAudioProcessingObject_Vtbl,
    rt: *const IAudioProcessingObjectRT_Vtbl,
    config: *const IAudioProcessingObjectConfiguration_Vtbl,
    effects: *const IAudioSystemEffects_Vtbl,
    /// Non-delegating IUnknown: owns the reference count.
    inner: *const IUnknown_Vtbl,
    refs: AtomicU32,
    /// Controlling unknown: the engine's outer object when aggregated,
    /// otherwise `&inner`. Not reference-counted (COM aggregation rule).
    outer: *mut c_void,
    core: Core,
}

const OFF_APO: usize = offset_of!(ApoObject, apo);
const OFF_RT: usize = offset_of!(ApoObject, rt);
const OFF_CONFIG: usize = offset_of!(ApoObject, config);
const OFF_EFFECTS: usize = offset_of!(ApoObject, effects);
const OFF_INNER: usize = offset_of!(ApoObject, inner);

/// # Safety
/// `this` must be an interface pointer handed out by this object for the
/// field at `OFF`.
unsafe fn object<'a, const OFF: usize>(this: *mut c_void) -> &'a ApoObject {
    unsafe { &*((this as *mut u8).sub(OFF) as *const ApoObject) }
}

unsafe fn unknown_vtbl<'a>(this: *mut c_void) -> &'a IUnknown_Vtbl {
    unsafe { &**(this as *const *const IUnknown_Vtbl) }
}

fn to_hresult(result: Result<()>) -> HRESULT {
    match result {
        Ok(()) => S_OK,
        Err(error) => error.code(),
    }
}

// Delegating IUnknown for every public interface.
unsafe extern "system" fn delegating_query<const OFF: usize>(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        let outer = object::<OFF>(this).outer;
        (unknown_vtbl(outer).QueryInterface)(outer, iid, out)
    }
}

unsafe extern "system" fn delegating_add_ref<const OFF: usize>(this: *mut c_void) -> u32 {
    unsafe {
        let outer = object::<OFF>(this).outer;
        (unknown_vtbl(outer).AddRef)(outer)
    }
}

unsafe extern "system" fn delegating_release<const OFF: usize>(this: *mut c_void) -> u32 {
    unsafe {
        let outer = object::<OFF>(this).outer;
        (unknown_vtbl(outer).Release)(outer)
    }
}

const fn delegating_unknown<const OFF: usize>() -> IUnknown_Vtbl {
    IUnknown_Vtbl {
        QueryInterface: delegating_query::<OFF>,
        AddRef: delegating_add_ref::<OFF>,
        Release: delegating_release::<OFF>,
    }
}

// Non-delegating (inner) IUnknown.
unsafe extern "system" fn inner_query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    unsafe {
        *out = ptr::null_mut();
        let object = object::<OFF_INNER>(this) as *const ApoObject as *mut ApoObject;
        let iid = *iid;
        let interface: *mut c_void = if iid == IUnknown::IID {
            ptr::addr_of_mut!((*object).inner) as *mut c_void
        } else if iid == IAudioProcessingObject::IID {
            ptr::addr_of_mut!((*object).apo) as *mut c_void
        } else if iid == IAudioProcessingObjectRT::IID {
            ptr::addr_of_mut!((*object).rt) as *mut c_void
        } else if iid == IAudioProcessingObjectConfiguration::IID {
            ptr::addr_of_mut!((*object).config) as *mut c_void
        } else if iid == IAudioSystemEffects::IID {
            ptr::addr_of_mut!((*object).effects) as *mut c_void
        } else {
            return E_NOINTERFACE;
        };
        // AddRef through the returned interface: for the public interfaces
        // this reaches the controlling unknown, as aggregation requires.
        (unknown_vtbl(interface).AddRef)(interface);
        *out = interface;
    }
    S_OK
}

unsafe extern "system" fn inner_add_ref(this: *mut c_void) -> u32 {
    unsafe {
        object::<OFF_INNER>(this)
            .refs
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}

unsafe extern "system" fn inner_release(this: *mut c_void) -> u32 {
    unsafe {
        let object = object::<OFF_INNER>(this);
        let remaining = object.refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if remaining == 0 {
            drop(Box::from_raw(object as *const ApoObject as *mut ApoObject));
        }
        remaining
    }
}

static INNER_VTBL: IUnknown_Vtbl = IUnknown_Vtbl {
    QueryInterface: inner_query,
    AddRef: inner_add_ref,
    Release: inner_release,
};

// IAudioProcessingObject
unsafe extern "system" fn apo_reset(this: *mut c_void) -> HRESULT {
    unsafe { to_hresult(object::<OFF_APO>(this).core.reset()) }
}

unsafe extern "system" fn apo_get_latency(this: *mut c_void, time: *mut i64) -> HRESULT {
    if time.is_null() {
        return E_POINTER;
    }
    unsafe { *time = object::<OFF_APO>(this).core.latency() };
    S_OK
}

unsafe extern "system" fn apo_get_registration_properties(
    _this: *mut c_void,
    out: *mut *mut APO_REG_PROPERTIES,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        let pointer =
            CoTaskMemAlloc(std::mem::size_of::<APO_REG_PROPERTIES>()) as *mut APO_REG_PROPERTIES;
        if pointer.is_null() {
            *out = ptr::null_mut();
            return E_OUTOFMEMORY;
        }
        pointer.write(registration_properties());
        *out = pointer;
    }
    S_OK
}

unsafe extern "system" fn apo_initialize(this: *mut c_void, size: u32, data: *const u8) -> HRESULT {
    unsafe { to_hresult(object::<OFF_APO>(this).core.initialize(size, data)) }
}

unsafe fn format_thunk(
    this: *mut c_void,
    which: &str,
    opposite: *mut c_void,
    requested: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        if !out.is_null() {
            *out = ptr::null_mut();
        }
        let opposite = IAudioMediaType::from_raw_borrowed(&opposite);
        let requested = IAudioMediaType::from_raw_borrowed(&requested);
        match object::<OFF_APO>(this)
            .core
            .format_supported(which, opposite, requested)
        {
            Ok(supported) => {
                if !out.is_null() {
                    *out = supported.into_raw();
                }
                S_OK
            }
            Err(error) => error.code(),
        }
    }
}

unsafe extern "system" fn apo_is_input_format_supported(
    this: *mut c_void,
    opposite: *mut c_void,
    requested: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe { format_thunk(this, "IsInputFormatSupported", opposite, requested, out) }
}

unsafe extern "system" fn apo_is_output_format_supported(
    this: *mut c_void,
    opposite: *mut c_void,
    requested: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    unsafe { format_thunk(this, "IsOutputFormatSupported", opposite, requested, out) }
}

unsafe extern "system" fn apo_get_input_channel_count(this: *mut c_void, out: *mut u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    unsafe {
        match object::<OFF_APO>(this).core.input_channel_count() {
            Ok(count) => {
                *out = count;
                S_OK
            }
            Err(error) => error.code(),
        }
    }
}

static APO_VTBL: IAudioProcessingObject_Vtbl = IAudioProcessingObject_Vtbl {
    base__: delegating_unknown::<OFF_APO>(),
    Reset: apo_reset,
    GetLatency: apo_get_latency,
    GetRegistrationProperties: apo_get_registration_properties,
    Initialize: apo_initialize,
    IsInputFormatSupported: apo_is_input_format_supported,
    IsOutputFormatSupported: apo_is_output_format_supported,
    GetInputChannelCount: apo_get_input_channel_count,
};

// IAudioProcessingObjectRT
unsafe extern "system" fn rt_process(
    this: *mut c_void,
    input_count: u32,
    inputs: *const *const APO_CONNECTION_PROPERTY,
    output_count: u32,
    outputs: *mut *mut APO_CONNECTION_PROPERTY,
) {
    unsafe {
        object::<OFF_RT>(this)
            .core
            .process(input_count, inputs, output_count, outputs)
    }
}

unsafe extern "system" fn rt_calc_input_frames(_this: *mut c_void, frames: u32) -> u32 {
    frames
}

unsafe extern "system" fn rt_calc_output_frames(_this: *mut c_void, frames: u32) -> u32 {
    frames
}

static RT_VTBL: IAudioProcessingObjectRT_Vtbl = IAudioProcessingObjectRT_Vtbl {
    base__: delegating_unknown::<OFF_RT>(),
    APOProcess: rt_process,
    CalcInputFrames: rt_calc_input_frames,
    CalcOutputFrames: rt_calc_output_frames,
};

// IAudioProcessingObjectConfiguration
unsafe extern "system" fn config_lock(
    this: *mut c_void,
    input_count: u32,
    inputs: *const *const APO_CONNECTION_DESCRIPTOR,
    output_count: u32,
    outputs: *const *const APO_CONNECTION_DESCRIPTOR,
) -> HRESULT {
    unsafe {
        let core = &object::<OFF_CONFIG>(this).core;
        let result = core.lock(input_count, inputs, output_count, outputs);
        let state = core.state();
        settings::trace(&format!(
            "LockForProcess: {input_count} in / {output_count} out -> {} ({} ch, {} Hz, max {} frames)",
            match &result {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("failed {:?}", e.code()),
            },
            state.channels,
            state.rate,
            state.max_frames
        ));
        if let Err(error) = &result {
            settings::trace_error(&format!(
                "Windows audio rejected the stream format: {:?}",
                error.code()
            ));
        }
        to_hresult(result)
    }
}

unsafe extern "system" fn config_unlock(this: *mut c_void) -> HRESULT {
    unsafe { to_hresult(object::<OFF_CONFIG>(this).core.unlock()) }
}

static CONFIG_VTBL: IAudioProcessingObjectConfiguration_Vtbl =
    IAudioProcessingObjectConfiguration_Vtbl {
        base__: delegating_unknown::<OFF_CONFIG>(),
        LockForProcess: config_lock,
        UnlockForProcess: config_unlock,
    };

// IAudioSystemEffects (marker interface, no methods)
static EFFECTS_VTBL: IAudioSystemEffects_Vtbl = IAudioSystemEffects_Vtbl {
    base__: delegating_unknown::<OFF_EFFECTS>(),
};

/// Creates the APO, optionally aggregated inside `outer`, and returns the
/// interface `riid` in `out`. When aggregated, COM requires `riid` to be
/// `IUnknown` (the caller receives our non-delegating unknown).
///
/// # Safety
/// `riid`/`out` must be valid; `outer` must be null or a live IUnknown.
pub unsafe fn create_instance(
    outer: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || riid.is_null() {
        return E_POINTER;
    }
    unsafe {
        *out = ptr::null_mut();
        if !outer.is_null() && *riid != IUnknown::IID {
            return CLASS_E_NOAGGREGATION;
        }
        let object = Box::into_raw(Box::new(ApoObject {
            apo: &APO_VTBL,
            rt: &RT_VTBL,
            config: &CONFIG_VTBL,
            effects: &EFFECTS_VTBL,
            inner: &INNER_VTBL,
            refs: AtomicU32::new(1),
            outer: ptr::null_mut(),
            core: Core {
                state: UnsafeCell::new(State::default()),
            },
        }));
        let inner = ptr::addr_of_mut!((*object).inner) as *mut c_void;
        (*object).outer = if outer.is_null() { inner } else { outer };
        let hr = inner_query(inner, riid, out);
        // Drop the creation reference; frees the object if the query failed.
        inner_release(inner);
        hr
    }
}

// ---------------------------------------------------------------------------
// Class factory and DLL exports
// ---------------------------------------------------------------------------

#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        settings::start_watcher();
        let outer = punkouter
            .as_ref()
            .map(|unknown| unknown.as_raw())
            .unwrap_or(ptr::null_mut());
        let hr = unsafe { create_instance(outer, riid, ppvobject) };
        settings::trace(&format!(
            "CreateInstance: aggregated {} riid {:?} -> {:?}",
            !outer.is_null(),
            if riid.is_null() {
                GUID::zeroed()
            } else {
                unsafe { *riid }
            },
            hr
        ));
        hr.ok()
    }

    fn LockServer(&self, _flock: BOOL) -> Result<()> {
        Ok(())
    }
}

/// COM entry point used by audiodg.exe (via CoCreateInstance).
///
/// # Safety
/// Standard COM contract: valid pointers from the COM runtime.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || rclsid.is_null() || riid.is_null() {
        settings::trace("DllGetClassObject: null pointer");
        return E_POINTER;
    }
    unsafe {
        *ppv = ptr::null_mut();
        let hr = if *rclsid != CLSID {
            CLASS_E_CLASSNOTAVAILABLE
        } else {
            let factory: IClassFactory = Factory.into();
            factory.query(riid, ppv)
        };
        settings::trace(&format!(
            "DllGetClassObject: clsid {:?} riid {:?} -> {:?}",
            *rclsid, *riid, hr
        ));
        hr
    }
}

/// The settings watcher thread lives for the life of the process, so the DLL
/// is never unloaded.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
