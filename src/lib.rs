#![deny(unsafe_op_in_unsafe_fn)]

//! Shared helpers for the Hush executables. The audio processing
//! itself lives in the `hush-apo` crate.

pub mod virtual_output;

pub const APP_NAME: &str = "Hush";
