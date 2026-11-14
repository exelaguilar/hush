//! Resolve privileged setup paths independently of the working directory
//! and environment inherited from the caller.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
};
use winreg::{
    enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY},
    RegKey,
};

pub fn program_files() -> io::Result<PathBuf> {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion",
        KEY_READ | KEY_WOW64_64KEY,
    )?;
    let path = PathBuf::from(key.get_value::<OsString, _>("ProgramFilesDir")?);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(io::Error::other("Program Files path is not absolute"))
    }
}

pub fn system_tool(executable: &str) -> io::Result<Command> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemDirectoryW(buffer: *mut u16, length: u32) -> u32;
    }
    let mut path = vec![0_u16; 32_768];
    let length = unsafe { GetSystemDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= path.len() {
        return Err(io::Error::other(
            "Windows system directory path is too long",
        ));
    }
    let path = PathBuf::from(OsString::from_wide(&path[..length]));
    let mut command = Command::new(path.join(executable));
    command.current_dir(&path).env("PATH", &path);
    Ok(command)
}

/// Finish writing a payload before replacing the previous installed file.
pub fn replace_file(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    let written = write(&mut file).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
