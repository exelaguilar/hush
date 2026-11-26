//! Resolve privileged setup paths independently of the working directory
//! and environment inherited from the caller.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    os::windows::{ffi::OsStringExt, fs::MetadataExt},
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

pub fn program_data() -> io::Result<PathBuf> {
    // The shell's machine-wide path avoids trusting the elevated caller's environment.
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetFolderPathW(
            hwnd: isize,
            folder: i32,
            token: isize,
            flags: u32,
            out: *mut u16,
        ) -> i32;
    }
    let mut buffer = [0_u16; 260];
    let result = unsafe { SHGetFolderPathW(0, 0x23, 0, 0, buffer.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::other(format!(
            "resolving ProgramData failed ({result:#x})"
        )));
    }
    let len =
        OsString::from_wide(&buffer[..buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len())]);
    let result = PathBuf::from(len);
    if !result.is_absolute() || result.as_os_str().encode_wide().count() == 0 {
        return Err(io::Error::other("ProgramData path is not absolute"));
    }
    Ok(result)
}

pub fn ensure_plain_dir(path: &Path) -> io::Result<()> {
    const REPARSE_POINT: u32 = 0x400;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(io::Error::other(format!(
            "{} is not a plain directory",
            path.display()
        )));
    }
    Ok(())
}

/// How long Setup, the installer and the SYSTEM repair tasks wait for another
/// privileged run to finish. A scheduled repair must still fit in its
/// 2-minute task limit after waiting.
pub const SETUP_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

pub fn setup_lock_path() -> io::Result<PathBuf> {
    Ok(program_files()?.join(".hush-setup.lock"))
}

pub fn setup_lock() -> io::Result<File> {
    lock_file(&setup_lock_path()?, SETUP_LOCK_WAIT)
}

/// Opens `path` and takes its exclusive lock, waiting up to `wait` for the
/// current holder instead of failing at once.
fn lock_file(path: &Path, wait: std::time::Duration) -> io::Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn pin_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(3)
        .custom_flags(0x0220_0000)
        .open(path)?;
    ensure_plain_dir(path)?;
    Ok(handle)
}

/// Reject locations whose owner or writable ACL entries permit an untrusted user
/// to replace executable code used by Setup or the SYSTEM tasks.
pub fn ensure_protected(path: &Path, check_writers: bool) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    type Raw = *mut c_void;
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn GetNamedSecurityInfoW(
            name: *const u16,
            kind: u32,
            parts: u32,
            owner: *mut Raw,
            group: *mut Raw,
            dacl: *mut Raw,
            sacl: *mut Raw,
            descriptor: *mut Raw,
        ) -> u32;
        fn ConvertStringSidToSidW(text: *const u16, sid: *mut Raw) -> i32;
        fn IsWellKnownSid(sid: Raw, kind: u32) -> i32;
        fn EqualSid(left: Raw, right: Raw) -> i32;
        fn GetAce(acl: Raw, index: u32, ace: *mut Raw) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: Raw) -> Raw;
    }
    struct Allocated(Raw);
    impl Drop for Allocated {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }
    if fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other("elevated setup path is a reparse point"));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut owner, mut dacl, mut descriptor) = (ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            1,
            5,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let _descriptor = Allocated(descriptor);
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let sid: Vec<u16> = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut trusted_installer = Allocated(ptr::null_mut());
    if unsafe { ConvertStringSidToSidW(sid.as_ptr(), &mut trusted_installer.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let trusted = |sid: Raw| {
        !sid.is_null()
            && unsafe {
                IsWellKnownSid(sid, 22) != 0
                    || IsWellKnownSid(sid, 26) != 0
                    || EqualSid(sid, trusted_installer.0) != 0
            }
    };
    let unprotected = || {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not protected from unprivileged writes",
                path.display()
            ),
        )
    };
    if !trusted(owner) || dacl.is_null() {
        return Err(unprotected());
    }
    if check_writers {
        let count = unsafe { dacl.cast::<u16>().add(2).read_unaligned() };
        for index in 0..u32::from(count) {
            let mut ace: Raw = ptr::null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let ace = ace.cast::<u8>();
            let (kind, flags) = unsafe { (*ace, *ace.add(1)) };
            if flags & 0x08 != 0 || !matches!(kind, 0 | 5 | 9 | 11) {
                continue;
            }
            let mask = unsafe { ace.add(4).cast::<u32>().read_unaligned() };
            const WRITE: u32 = 0x500d_0156;
            if mask & WRITE != 0 && (kind != 0 || !trusted(unsafe { ace.add(8).cast() })) {
                return Err(unprotected());
            }
        }
    }
    Ok(())
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

pub fn stop_installed_app(executable: &Path) -> io::Result<()> {
    let name = executable
        .file_stem()
        .ok_or_else(|| io::Error::other("invalid app path"))?;
    let script = r#"$ErrorActionPreference = 'Stop'; Get-Process -Name $env:HUSH_PROCESS_NAME -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $env:HUSH_INSTALLED_EXE } | Stop-Process -Force"#;
    let status = system_tool(r"WindowsPowerShell\v1.0\powershell.exe")?
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("HUSH_PROCESS_NAME", name)
        .env("HUSH_INSTALLED_EXE", executable)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            "Could not stop the installed Hush process",
        ))
    }
}

pub fn audio_services(start: bool) -> io::Result<()> {
    use std::{
        os::windows::ffi::OsStrExt,
        thread,
        time::{Duration, Instant},
    };
    #[repr(C)]
    #[derive(Default)]
    struct Status {
        kind: u32,
        state: u32,
        controls: u32,
        exit: u32,
        service_exit: u32,
        checkpoint: u32,
        wait_hint: u32,
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenSCManagerW(machine: *const u16, database: *const u16, rights: u32) -> isize;
        fn OpenServiceW(manager: isize, name: *const u16, rights: u32) -> isize;
        fn QueryServiceStatus(service: isize, status: *mut Status) -> i32;
        fn ControlService(service: isize, command: u32, status: *mut Status) -> i32;
        fn StartServiceW(service: isize, argc: u32, argv: *const *const u16) -> i32;
        fn CloseServiceHandle(handle: isize) -> i32;
    }
    struct Service(isize);
    impl Drop for Service {
        fn drop(&mut self) {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
    fn handle(raw: isize) -> io::Result<Service> {
        if raw == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Service(raw))
        }
    }
    const STOPPED: u32 = 1;
    const START_PENDING: u32 = 2;
    const STOP_PENDING: u32 = 3;
    const RUNNING: u32 = 4;
    let manager = handle(unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), 1) })?;
    let names = if start {
        ["AudioEndpointBuilder", "Audiosrv"]
    } else {
        ["Audiosrv", "AudioEndpointBuilder"]
    };
    for name in names {
        let wide: Vec<_> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let service = handle(unsafe {
            OpenServiceW(
                manager.0,
                wide.as_ptr(),
                0x0004 | if start { 0x0010 } else { 0x0020 },
            )
        })?;
        let mut status = Status::default();
        let deadline = Instant::now() + Duration::from_secs(20);
        let wanted = if start { RUNNING } else { STOPPED };
        let mut requested = false;
        loop {
            if unsafe { QueryServiceStatus(service.0, &mut status) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if status.state == wanted {
                break;
            }
            if !requested && !matches!(status.state, START_PENDING | STOP_PENDING) {
                let ok = if start {
                    unsafe { StartServiceW(service.0, 0, std::ptr::null()) }
                } else {
                    unsafe { ControlService(service.0, 1, &mut status) }
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    if !matches!(
                        (start, error.raw_os_error()),
                        (true, Some(1056)) | (false, Some(1062))
                    ) {
                        return Err(io::Error::other(format!("{name}: {error}")));
                    }
                }
                requested = true;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{name} did not reach the requested state"),
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::lock_file;
    use std::time::Duration;

    #[test]
    fn setup_lock_waits_for_the_current_holder() {
        let path = std::env::temp_dir().join(format!("hush-setup-lock-{}", std::process::id()));
        let holder = lock_file(&path, Duration::ZERO).unwrap();
        // A second run gives up when it may not wait...
        assert!(lock_file(&path, Duration::from_millis(50)).is_err());
        // ...and gets the lock once the holder finishes.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(holder);
        });
        assert!(lock_file(&path, Duration::from_secs(5)).is_ok());
        release.join().unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn held_setup_lock_can_be_removed_by_its_holder() {
        let path = std::env::temp_dir().join(format!("hush-setup-unlock-{}", std::process::id()));
        let holder = lock_file(&path, Duration::ZERO).unwrap();
        std::fs::remove_file(&path).unwrap();
        drop(holder);
        assert!(!path.exists());
    }
}
