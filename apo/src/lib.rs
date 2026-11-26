//! Hush clean-mic Audio Processing Object (APO).
//!
//! Windows' audio engine (audiodg.exe) loads this DLL for the microphone it
//! is registered on and runs every captured buffer through
//! [`dsp::CleanMic`]. Applications keep recording from the normal
//! microphone and receive the cleaned audio. No virtual device and no kernel
//! driver are involved.

// rustc exports DllGetClassObject/DllCanUnloadNow itself; the MSVC linker
// notes they "should be PRIVATE" (an import-library nicety, harmless here).
#![allow(linker_messages)]

pub mod denoise;
#[cfg(any(test, feature = "offline-experiments"))]
mod dereverb;
pub mod dsp;
mod protection;
pub mod settings;

#[cfg(windows)]
mod apo;
#[cfg(windows)]
pub use apo::{create_instance, DllCanUnloadNow, DllGetClassObject, CLSID};

/// COM class ID of the clean-mic APO, as registered by `hush-setup`.
pub const CLSID_STRING: &str = "{8AA11902-CC87-49EB-B601-AA1449A395A6}";
pub const FRIENDLY_NAME: &str = "Hush Clean Mic";
pub const COPYRIGHT: &str = "Copyright (c) 2026 Exel";
pub const DLL_NAME: &str = "hush_apo.dll";
