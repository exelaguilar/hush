//! Authenticated machine state used by Setup and its SYSTEM tasks.

use std::io;
use winreg::{
    enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
    RegKey,
};

const KEY: &str = r"SOFTWARE\Exel\Hush";

pub const INSTALL_RECORD: &str = "Endpoints";
pub const REPAIR_REPORT: &str = "RepairReport";

pub fn read(value: &str) -> io::Result<String> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(KEY, KEY_READ | KEY_WOW64_64KEY)?
        .get_value(value)
}

#[allow(dead_code)]
pub fn write(value: &str, text: &str) -> io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey_with_flags(KEY, winreg::enums::KEY_WRITE | KEY_WOW64_64KEY)?;
    key.set_value(value, &text)
}

#[allow(dead_code)]
pub fn delete(value: &str) -> io::Result<()> {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(KEY, winreg::enums::KEY_SET_VALUE | KEY_WOW64_64KEY)?;
    key.delete_value(value)
}
