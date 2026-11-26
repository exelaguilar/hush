#![deny(unsafe_op_in_unsafe_fn)]

//! Shared helpers for the Hush executables. The audio processing
//! itself lives in the `hush-apo` crate.

pub mod virtual_output;

#[cfg(windows)]
pub mod machine_state;

#[cfg(windows)]
pub mod repair_report;

#[cfg(windows)]
pub mod device_registry;

#[cfg(windows)]
pub mod windows_setup_paths;

pub const APP_NAME: &str = "Hush";
