//! Installs / removes the Hush clean-mic APO on one microphone.
//!
//!   hush-setup list                     audio endpoints and their effects (read-only)
//!   hush-setup status                   install state + whether the APO is running
//!   hush-setup install --mic <name>     attach the APO (asks for admin once)
//!   hush-setup uninstall                remove everything this tool added
//!   hush-setup repair [--restart-audio] [--quiet]
//!                                       re-attach the APO after a replug or a
//!                                       driver rewrite (admin; also run by the
//!                                       on-demand scheduled tasks as SYSTEM)
//!   hush-setup repair-task-status       whether the repair tasks exist
//!
//! Options: --slot efx|gfx|lfx (default efx), --no-restart (skip restarting the
//! Windows Audio service), --purge (uninstall: also delete settings).
//!
//! What install changes, and nothing else:
//!   * copies hush_apo.dll to %ProgramFiles%\Hush
//!   * registers its COM class + APO entry under HKLM\SOFTWARE\Classes
//!   * adds 1-2 values to the chosen endpoint's FxProperties key, only if
//!     those values are empty (another product's effect is never replaced)
//!   * creates %ProgramData%\Hush (settings, status, install record)
//!   * restarts the Windows Audio service so the change takes effect
//!
//! Every value written is recorded in installed.txt; uninstall removes exactly
//! those entries if they still hold Hush's data.

#[cfg(not(windows))]
fn main() {
    eprintln!("hush-setup is Windows-only");
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_main::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
#[path = "../windows_setup_paths.rs"]
mod windows_setup_paths;

#[cfg(windows)]
mod windows_main {
    use super::windows_setup_paths::{program_files, replace_file, system_tool};
    use anyhow::{anyhow, bail, Context, Result};
    use hush_apo::{settings, CLSID_STRING, COPYRIGHT, DLL_NAME, FRIENDLY_NAME};
    use std::{
        fs,
        os::windows::process::CommandExt,
        path::{Path, PathBuf},
    };
    use winreg::{
        enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE},
        types::ToRegValue,
        RegKey,
    };

    const MMDEVICES: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio";
    const PKEY_NAME: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
    const PKEY_DESCRIPTION: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";
    const PKEY_DISABLE_SYSFX: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
    const FX: &str = "{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}";
    const FX_MODES: &str = "{d3993a3f-99c2-4402-b5ec-a92a0367664b}";
    /// Processing modes the endpoint effect runs in. Only the default mode,
    /// exactly as SteelSeries Sonar's own endpoint effects are registered on
    /// this machine (a configuration Windows demonstrably honours).
    const MODES: [&str; 1] = ["{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}"];
    const IID_IAUDIOPROCESSINGOBJECT: &str = "{FD7F2B29-24D0-4b5c-B177-592C39F9CA10}";
    const INSTALL_RECORD: &str = "installed.txt";

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Flow {
        Capture,
        Render,
    }

    impl Flow {
        fn key(self) -> &'static str {
            match self {
                Flow::Capture => "Capture",
                Flow::Render => "Render",
            }
        }
        fn parse(text: &str) -> Option<Self> {
            match text {
                "Capture" => Some(Flow::Capture),
                "Render" => Some(Flow::Render),
                _ => None,
            }
        }
    }

    struct Endpoint {
        flow: Flow,
        id: String,
        name: String,
        active: bool,
        effects_disabled: bool,
    }

    fn hklm() -> RegKey {
        RegKey::predef(HKEY_LOCAL_MACHINE)
    }

    fn endpoints(flow: Flow) -> Result<Vec<Endpoint>> {
        let root = hklm()
            .open_subkey_with_flags(format!(r"{MMDEVICES}\{}", flow.key()), KEY_READ)
            .context("reading audio endpoints")?;
        let mut list = Vec::new();
        for id in root.enum_keys().flatten() {
            let Ok(key) = root.open_subkey_with_flags(&id, KEY_READ) else {
                continue;
            };
            let state: u32 = key.get_value("DeviceState").unwrap_or(0);
            let props = key.open_subkey_with_flags("Properties", KEY_READ).ok();
            let get = |name: &str| -> String {
                props
                    .as_ref()
                    .and_then(|p| p.get_value::<String, _>(name).ok())
                    .unwrap_or_default()
            };
            let (short, description) = (get(PKEY_NAME), get(PKEY_DESCRIPTION));
            let name = if description.is_empty() {
                short
            } else {
                format!("{short} ({description})")
            };
            let effects_disabled = props
                .as_ref()
                .and_then(|p| p.get_value::<u32, _>(PKEY_DISABLE_SYSFX).ok())
                .is_some_and(|v| v == 1);
            list.push(Endpoint {
                flow,
                id,
                name,
                active: state == 1,
                effects_disabled,
            });
        }
        list.sort_by_key(|e| e.name.to_lowercase());
        Ok(list)
    }

    fn fx_path(flow: Flow, id: &str) -> String {
        format!(r"{MMDEVICES}\{}\{id}\FxProperties", flow.key())
    }

    fn fx_values(flow: Flow, id: &str) -> Vec<(String, String)> {
        let Ok(key) = hklm().open_subkey_with_flags(fx_path(flow, id), KEY_READ) else {
            return Vec::new();
        };
        key.enum_values()
            .flatten()
            .map(|(name, value)| (name, value.to_string()))
            .collect()
    }

    fn is_admin() -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, HANDLE},
            Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        unsafe {
            let mut token = HANDLE::default();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut bytes = 0;
            let result = GetTokenInformation(
                token,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut bytes,
            );
            let _ = CloseHandle(token);
            result.is_ok() && elevation.TokenIsElevated != 0
        }
    }

    /// Re-runs this exe elevated (one UAC prompt) and shows its output here.
    fn relaunch_elevated(args: &[String]) -> Result<()> {
        let exe = std::env::current_exe()?;
        let log = std::env::temp_dir().join("hush-setup.log");
        let _ = fs::remove_file(&log);
        let mut forwarded: Vec<String> = args.to_vec();
        forwarded.push("--log".into());
        forwarded.push(log.display().to_string());
        let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let arg_list = forwarded
            .iter()
            .map(|a| quote(&format!("\"{a}\"")))
            .collect::<Vec<_>>()
            .join(",");
        let script = format!(
            "$p = Start-Process -FilePath {} -ArgumentList {} -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode",
            quote(&exe.display().to_string()),
            arg_list
        );
        println!("Requesting administrator permission (Windows will ask)...");
        let status = system_tool(r"WindowsPowerShell\v1.0\powershell.exe")?
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            // No console window when started from the app or installer.
            .creation_flags(0x0800_0000)
            .status()
            .context("launching elevated setup")?;
        if let Ok(text) = fs::read_to_string(&log) {
            print!("{text}");
        }
        if !status.success() {
            bail!("elevated setup did not complete (cancelled or failed)");
        }
        Ok(())
    }

    struct Out {
        log: Option<PathBuf>,
        /// Log to the file only, not stdout.
        mute: bool,
    }

    impl Out {
        fn line(&self, text: impl AsRef<str>) {
            let text = text.as_ref();
            if !self.mute {
                println!("{text}");
            }
            if let Some(log) = &self.log {
                use std::io::Write;
                if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(log) {
                    let _ = writeln!(file, "{text}");
                }
            }
        }
    }

    fn program_dir() -> std::io::Result<PathBuf> {
        Ok(program_files()?.join("Hush"))
    }

    /// Stops or starts Windows audio. Stopping goes through
    /// AudioEndpointBuilder (which also stops its dependent, Audiosrv):
    /// AudioEndpointBuilder caches each endpoint's FxProperties, so restarting
    /// only Audiosrv does not pick up a newly attached effect. Starting Audiosrv
    /// starts AudioEndpointBuilder first.
    fn audio_service(out: &Out, start: bool) -> Result<()> {
        let (verb, service, label) = if start {
            ("start", "audiosrv", "starting Windows Audio")
        } else {
            (
                "stop",
                "audioendpointbuilder",
                "stopping Windows Audio (and Endpoint Builder)",
            )
        };
        out.line(format!("  {label}"));
        let mut command = system_tool("net.exe")?;
        command.args([verb, service]);
        if !start {
            command.arg("/y");
        }
        let output = command.output().context("running net")?;
        let text = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        // "already started" / "not started" are fine.
        if !output.status.success() && !text.contains("already") && !text.contains("not started") {
            bail!("net {verb} {service} failed: {}", text.trim());
        }
        Ok(())
    }

    fn with_audio_restart(
        out: &Out,
        restart: bool,
        change: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        if !restart {
            return change();
        }
        let result = audio_service(out, false).and_then(|()| change());
        match (result, audio_service(out, true)) {
            (Err(change), Err(start)) => {
                Err(change.context(format!("Windows Audio also failed to restart: {start:#}")))
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn register_com(dll: &Path) -> Result<()> {
        let (clsid, _) = hklm().create_subkey(format!(r"SOFTWARE\Classes\CLSID\{CLSID_STRING}"))?;
        clsid.set_value("", &FRIENDLY_NAME.to_string())?;
        let (server, _) = clsid.create_subkey("InprocServer32")?;
        server.set_value("", &dll.display().to_string())?;
        server.set_value("ThreadingModel", &"Both".to_string())?;

        let (apo, _) = hklm().create_subkey(format!(
            r"SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\{CLSID_STRING}"
        ))?;
        apo.set_value("FriendlyName", &FRIENDLY_NAME.to_string())?;
        apo.set_value("Copyright", &COPYRIGHT.to_string())?;
        apo.set_value("MajorVersion", &1_u32)?;
        apo.set_value("MinorVersion", &0_u32)?;
        apo.set_value("Flags", &15_u32)?; // APO_FLAG_DEFAULT | APO_FLAG_INPLACE
        apo.set_value("MinInputConnections", &1_u32)?;
        apo.set_value("MaxInputConnections", &1_u32)?;
        apo.set_value("MinOutputConnections", &1_u32)?;
        apo.set_value("MaxOutputConnections", &1_u32)?;
        apo.set_value("MaxInstances", &u32::MAX)?;
        apo.set_value("NumAPOInterfaces", &1_u32)?;
        apo.set_value("APOInterface0", &IID_IAUDIOPROCESSINGOBJECT.to_string())?;
        Ok(())
    }

    fn unregister_com() {
        let _ = hklm().delete_subkey_all(format!(r"SOFTWARE\Classes\CLSID\{CLSID_STRING}"));
        let _ = hklm().delete_subkey_all(format!(
            r"SOFTWARE\Classes\AudioEngine\AudioProcessingObjects\{CLSID_STRING}"
        ));
    }

    fn record_path() -> PathBuf {
        settings::settings_dir().join(INSTALL_RECORD)
    }

    /// One FX value this tool created. `label` (the endpoint's friendly name
    /// when it was installed) is an optional 4th column; older records lack it.
    #[derive(Clone, Debug)]
    struct RecLine {
        flow: Flow,
        id: String,
        value: String,
        label: Option<String>,
    }

    impl RecLine {
        fn render(&self) -> String {
            match &self.label {
                Some(label) => format!("{}\t{}\t{}\t{label}\n", self.flow.key(), self.id, self.value),
                None => format!("{}\t{}\t{}\n", self.flow.key(), self.id, self.value),
            }
        }
    }

    fn parse_record(text: &str) -> Vec<RecLine> {
        text.lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let flow = Flow::parse(parts.next()?)?;
                Some(RecLine {
                    flow,
                    id: parts.next()?.to_string(),
                    value: parts.next()?.to_string(),
                    label: parts.next().filter(|l| !l.is_empty()).map(str::to_string),
                })
            })
            .collect()
    }

    fn read_record() -> Vec<RecLine> {
        parse_record(&fs::read_to_string(record_path()).unwrap_or_default())
    }

    fn find_mic(query: &str) -> Result<Endpoint> {
        let query_lower = query.to_lowercase();
        let mut active: Vec<_> = endpoints(Flow::Capture)?
            .into_iter()
            .filter(|e| e.active)
            .collect();
        if let Some(index) = active.iter().position(|e| e.id.eq_ignore_ascii_case(query)) {
            return Ok(active.swap_remove(index));
        }
        let matches: Vec<_> = active
            .into_iter()
            .filter(|e| e.name.to_lowercase().contains(&query_lower))
            .collect();
        match matches.len() {
            0 => bail!("no active microphone matches \"{query}\" (run: hush-setup list)"),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => bail!(
                "\"{query}\" matches several microphones: {}",
                matches
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    struct Attachment {
        id: String,
        key: RegKey,
        values: Vec<(String, winreg::RegValue)>,
    }

    fn raw_value(key: &RegKey, name: &str) -> std::io::Result<Option<winreg::RegValue>> {
        match key.get_raw_value(name) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn attachments(flow: Flow, except: Option<&str>) -> Result<Vec<Attachment>> {
        let mut found = Vec::new();
        for endpoint in endpoints(flow)? {
            if except == Some(endpoint.id.as_str()) {
                continue;
            }
            let path = fx_path(flow, &endpoint.id);
            let read = match hklm().open_subkey_with_flags(&path, KEY_READ) {
                Ok(key) => key,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| format!("reading {path}")),
            };
            let mut values = Vec::new();
            for slot in [1, 2, 7] {
                let name = format!("{FX},{slot}");
                if let Some(value) = raw_value(&read, &name)? {
                    if value.to_string().eq_ignore_ascii_case(CLSID_STRING) {
                        values.push((name, value));
                        if slot == 7 {
                            let name = format!("{FX_MODES},7");
                            let modes: Vec<_> = MODES.iter().map(|mode| mode.to_string()).collect();
                            if let Some(value) =
                                raw_value(&read, &name)?.filter(|v| *v == modes.to_reg_value())
                            {
                                values.push((name, value));
                            }
                        }
                    }
                }
            }
            if !values.is_empty() {
                let key = hklm()
                    .open_subkey_with_flags(&path, KEY_READ | KEY_SET_VALUE)
                    .with_context(|| format!("opening {path} for detach"))?;
                found.push(Attachment {
                    id: endpoint.id.clone(),
                    key,
                    values,
                });
            }
        }
        Ok(found)
    }

    struct FxChange<'a> {
        key: &'a RegKey,
        name: &'a str,
        before: Option<&'a winreg::RegValue>,
        after: Option<&'a winreg::RegValue>,
    }

    fn write_fx(key: &RegKey, name: &str, value: Option<&winreg::RegValue>) -> std::io::Result<()> {
        match value {
            Some(value) => key.set_raw_value(name, value),
            None => key.delete_value(name),
        }
    }

    fn apply_fx(changes: &[FxChange<'_>], finish: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut applied = 0;
        let result = (|| {
            for change in changes {
                if raw_value(change.key, change.name)?.as_ref() != change.before {
                    bail!(
                        "microphone effect slot changed during setup: {}",
                        change.name
                    );
                }
                write_fx(change.key, change.name, change.after)?;
                applied += 1;
            }
            finish()
        })();
        if let Err(error) = result {
            let mut rollback_error = None;
            for change in changes[..applied].iter().rev() {
                let restored = raw_value(change.key, change.name).and_then(|current| {
                    if current.as_ref() != change.after {
                        return Err(std::io::Error::other("effect slot changed during rollback"));
                    }
                    write_fx(change.key, change.name, change.before)
                });
                if let Err(error) = restored {
                    rollback_error = Some(error);
                }
            }
            if let Some(rollback_error) = rollback_error {
                return Err(error.context(format!("effect rollback incomplete: {rollback_error}")));
            }
            return Err(error);
        }
        Ok(())
    }

    fn write_record(text: &str) -> Result<()> {
        use std::io::Write;
        replace_file(&record_path(), |file| file.write_all(text.as_bytes()))
            .context("saving the microphone installation record")
    }

    fn install(out: &Out, mic: &str, slot: &str, restart: bool, replace_mic: bool) -> Result<()> {
        let endpoint = find_mic(mic)?;
        install_endpoint(out, &endpoint, slot, restart, replace_mic, &[])?;
        // The installer runs this from a staging folder and then creates the
        // tasks with the installed copy (`create-repair-tasks`).
        if is_installed_copy() {
            if let Err(error) = create_repair_tasks(out) {
                out.line(format!(
                    "  warning: could not create the repair tasks ({error:#}); Hush will ask for permission to repair"
                ));
            }
        }
        out.line("Done. Open any app that uses the microphone, then run: hush-setup status");
        Ok(())
    }

    /// Attaches the APO to `endpoint`. `drop_ids` are other endpoints (the
    /// recorded, now-stale ones) whose Hush values and record lines are removed.
    /// Shared by `install` and `repair`.
    fn install_endpoint(
        out: &Out,
        endpoint: &Endpoint,
        slot: &str,
        restart: bool,
        replace_mic: bool,
        drop_ids: &[String],
    ) -> Result<()> {
        if endpoint.name.to_lowercase().contains("steelseries sonar")
            || endpoint.name.to_lowercase().contains("vb-audio")
        {
            bail!(
                "{} is another product's virtual device; choose your physical microphone",
                endpoint.name
            );
        }
        let clsid_value = match slot {
            "efx" => format!("{FX},7"),
            "lfx" => format!("{FX},1"),
            // Legacy global effect (post-mix): one instance per endpoint, the
            // slot Equalizer APO uses by default for third-party effects.
            "gfx" => format!("{FX},2"),
            other => bail!("unknown slot {other} (use efx, gfx or lfx)"),
        };
        let mut planned: Vec<(String, winreg::RegValue)> =
            vec![(clsid_value, CLSID_STRING.to_string().to_reg_value())];
        if slot == "efx" {
            let modes: Vec<String> = MODES.iter().map(|m| m.to_string()).collect();
            planned.push((format!("{FX_MODES},7"), modes.to_reg_value()));
        }

        out.line(format!("Installing Hush Clean Mic on: {}", endpoint.name));
        out.line(format!(
            "  endpoint {} ({} slot)",
            endpoint.id,
            slot.to_uppercase()
        ));
        if endpoint.effects_disabled {
            out.line("  WARNING: \"Audio enhancements\" are turned off for this microphone in Windows Sound settings; turn them on or the effect will not run.");
        }

        // Refuse to replace anything that is not ours.
        let fx = hklm()
            .open_subkey_with_flags(
                fx_path(endpoint.flow, &endpoint.id),
                KEY_READ | KEY_SET_VALUE,
            )
            .context("opening the microphone's FxProperties key for writing")?;
        let mut existing = Vec::new();
        for (name, value) in &planned {
            let previous = raw_value(&fx, name)?;
            if let Some(ref previous) = previous {
                if previous != value {
                    bail!(
                        "{name} is already used by another audio effect on this microphone ({}); not replacing it",
                        previous
                    );
                }
            }
            existing.push(previous);
        }
        let previous = if replace_mic {
            attachments(Flow::Capture, Some(&endpoint.id))?
        } else if !drop_ids.is_empty() {
            attachments(Flow::Capture, None)?
                .into_iter()
                .filter(|a| drop_ids.iter().any(|d| d.eq_ignore_ascii_case(&a.id)))
                .collect()
        } else {
            Vec::new()
        };

        let source = std::env::current_exe()?
            .parent()
            .ok_or_else(|| anyhow!("no exe directory"))?
            .join(DLL_NAME);
        if !source.exists() {
            bail!(
                "{} not found next to hush-setup.exe (run cargo build --release)",
                source.display()
            );
        }

        let settings_dir = settings::settings_dir();
        fs::create_dir_all(&settings_dir)?;
        // Users may edit settings (the app runs unelevated). The audio engine
        // runs as LOCAL SERVICE with a write-restricted token, so writing
        // status.txt also needs the WRITE RESTRICTED and Audiosrv service SIDs.
        for grant in [
            "*S-1-5-32-545:(OI)(CI)M",
            "*S-1-5-19:(OI)(CI)M",
            "*S-1-5-33:(OI)(CI)M",
            "NT SERVICE\\Audiosrv:(OI)(CI)M",
        ] {
            let status = system_tool("icacls.exe")?
                .arg(&settings_dir)
                .args(["/grant", grant])
                .stdout(std::process::Stdio::null())
                .status()?;
            if !status.success() {
                bail!("icacls {grant} failed on {}", settings_dir.display());
            }
        }
        let settings_file = settings::settings_path();
        if !settings_file.exists() {
            fs::write(&settings_file, settings::format(&Default::default()))?;
            out.line(format!("  created {}", settings_file.display()));
        }

        with_audio_restart(out, restart, || {
            let target_dir = program_dir()?;
            fs::create_dir_all(&target_dir)?;
            let target = target_dir.join(DLL_NAME);
            // When run from the installed folder the DLL is already in place.
            let same_file =
                fs::canonicalize(&source).ok() == fs::canonicalize(&target).ok() && target.exists();
            if !same_file {
                replace_file(&target, |file| {
                    std::io::copy(&mut fs::File::open(&source)?, file).map(|_| ())
                })
                .with_context(|| format!("copying DLL to {}", target.display()))?;
                out.line(format!("  copied {}", target.display()));
            }
            register_com(&target)?;
            out.line(format!("  registered COM class {CLSID_STRING}"));

            let mut record = String::new();
            for line in read_record() {
                let stale = (replace_mic && line.flow == Flow::Capture && line.id != endpoint.id)
                    || drop_ids.iter().any(|d| d.eq_ignore_ascii_case(&line.id));
                let rewritten = line.flow == endpoint.flow
                    && line.id == endpoint.id
                    && planned.iter().any(|(name, _)| *name == line.value);
                if !stale && !rewritten {
                    record.push_str(&line.render());
                }
            }
            let mut changes = Vec::new();
            for ((name, value), before) in planned.iter().zip(&existing) {
                changes.push(FxChange {
                    key: &fx,
                    name,
                    before: before.as_ref(),
                    after: Some(value),
                });
                record.push_str(
                    &RecLine {
                        flow: endpoint.flow,
                        id: endpoint.id.clone(),
                        value: name.clone(),
                        label: Some(endpoint.name.clone()).filter(|n| !n.is_empty()),
                    }
                    .render(),
                );
            }
            for attachment in &previous {
                for (name, value) in &attachment.values {
                    changes.push(FxChange {
                        key: &attachment.key,
                        name,
                        before: Some(value),
                        after: None,
                    });
                }
            }
            apply_fx(&changes, || write_record(&record))?;
            if !previous.is_empty() {
                out.line("  removed Clean Mic from the previous microphone");
            }
            Ok(())
        })
    }

    fn uninstall(out: &Out, restart: bool, purge: bool) -> Result<()> {
        out.line("Removing Hush Clean Mic");
        let mut installed = Vec::new();
        for flow in [Flow::Capture, Flow::Render] {
            installed.extend(attachments(flow, None)?);
        }
        let mut changes = Vec::new();
        for attachment in &installed {
            for (name, value) in &attachment.values {
                changes.push(FxChange {
                    key: &attachment.key,
                    name,
                    before: Some(value),
                    after: None,
                });
            }
        }
        with_audio_restart(out, restart, || {
            apply_fx(&changes, || Ok(()))?;
            unregister_com();
            out.line("  unregistered COM class");
            let dir = program_dir()?;
            let dll = dir.join(DLL_NAME);
            if dll.exists() {
                match fs::remove_file(&dll) {
                    Ok(()) => out.line(format!("  deleted {}", dll.display())),
                    Err(error) => out.line(format!(
                        "  could not delete {} ({error}); delete it after a reboot",
                        dll.display()
                    )),
                }
            }
            let _ = fs::remove_dir(&dir);
            Ok(())
        })?;
        delete_repair_tasks(out);
        let _ = fs::remove_file(record_path());
        if purge {
            let _ = fs::remove_dir_all(settings::settings_dir());
            out.line("  deleted settings");
        }
        out.line("Done.");
        Ok(())
    }

    fn list() -> Result<()> {
        for flow in [Flow::Capture, Flow::Render] {
            println!("{} endpoints:", flow.key());
            for endpoint in endpoints(flow)?.into_iter().filter(|e| e.active) {
                println!(
                    "  {}{}\n    id {}",
                    endpoint.name,
                    if endpoint.effects_disabled {
                        "  [audio enhancements OFF]"
                    } else {
                        ""
                    },
                    endpoint.id
                );
                for (name, value) in fx_values(flow, &endpoint.id) {
                    if name.starts_with(FX) || name.starts_with(FX_MODES) {
                        let mark = if value.contains(CLSID_STRING) {
                            " <- Hush"
                        } else {
                            ""
                        };
                        println!("    fx {name} = {}{mark}", value.replace('\n', " "));
                    }
                }
            }
        }
        Ok(())
    }

    fn status() -> Result<()> {
        let registered = hklm()
            .open_subkey_with_flags(
                format!(r"SOFTWARE\Classes\CLSID\{CLSID_STRING}\InprocServer32"),
                KEY_READ,
            )
            .and_then(|k| k.get_value::<String, _>(""))
            .ok();
        println!(
            "COM registration: {}",
            registered.as_deref().unwrap_or("not registered")
        );
        let record = read_record();
        if record.is_empty() {
            println!("Attached to: nothing");
        }
        for RecLine {
            flow,
            id,
            value: name,
            ..
        } in &record
        {
            let device = endpoints(*flow)
                .unwrap_or_default()
                .into_iter()
                .find(|e| e.id == *id)
                .map(|e| e.name)
                .unwrap_or_else(|| id.clone());
            let present = fx_values(*flow, id).iter().any(|(n, _)| n == name);
            println!(
                "Attached to: {device} [{name}]{}",
                if present { "" } else { "  (value missing!)" }
            );
        }
        repair_task_status();
        match read_repair_file() {
            Some(report) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                println!(
                    "Last repair: {} ({} s ago): {}",
                    report.result,
                    now.saturating_sub(report.time_unix),
                    report.message
                );
            }
            None => println!("Last repair: none"),
        }
        println!("Settings: {}", settings::settings_path().display());
        match fs::read_to_string(settings::status_path())
            .ok()
            .and_then(|t| settings::parse_status(&t))
        {
            Some(s) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let age = now.saturating_sub(s.updated_unix);
                println!(
                    "Engine: last report {age} s ago from process {}: {:.1} s of audio processed, {} Hz, {} ch{}",
                    s.process_id,
                    s.processed_frames as f64 / s.rate.max(1) as f64,
                    s.rate,
                    s.channels,
                    if age <= 3 { "  -> RUNNING" } else { "  (idle; open an app that records the mic)" }
                );
                println!(
                    "Glitches since audio started: {} late frames (gaps), {} samples skipped",
                    s.underruns, s.skipped
                );
            }
            None => println!("Engine: no report yet (the APO has not processed audio)"),
        }
        Ok(())
    }

    // ---- repair -----------------------------------------------------------

    const REPAIR_FILE: &str = "repair.txt";
    const SETUP_EXE: &str = "hush-setup.exe";
    /// (task name, arguments). On demand only, run as SYSTEM; fixed command
    /// lines with no user-controlled input.
    const REPAIR_TASKS: [(&str, &str); 2] = [
        (r"\Hush\Repair Clean Mic", "repair --quiet"),
        (r"\Hush\Restart Clean Mic", "repair --restart-audio --quiet"),
    ];
    /// Administrators and SYSTEM: full. Authenticated Users: read + execute
    /// (may run the task, may not change it).
    const TASK_SDDL: &str = "D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;GRGX;;;AU)";

    /// A recorded Capture endpoint and the slot Hush was installed in.
    #[derive(Clone, Debug, PartialEq)]
    struct RecMic {
        id: String,
        slot: String,
        label: Option<String>,
    }

    /// Current state of one Capture endpoint.
    #[derive(Clone, Debug)]
    struct Snap {
        id: String,
        name: String,
        active: bool,
        has_hush: bool,
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Step {
        /// Active and Hush is attached.
        Ok { id: String, name: String },
        /// Active but Hush's values are gone: write them again.
        Rewrite { id: String, name: String, slot: String },
        /// The mic came back as a different endpoint: attach there, forget `old`.
        Migrate { old: String, new: String, name: String, slot: String },
        /// Not present (or not decidable): leave it alone.
        Waiting { name: String, ambiguous: bool },
    }

    impl Step {
        fn changes(&self) -> bool {
            matches!(self, Step::Rewrite { .. } | Step::Migrate { .. })
        }
    }

    fn same_id(a: &str, b: &str) -> bool {
        a.eq_ignore_ascii_case(b)
    }

    /// Pure decision: what to do for each recorded microphone.
    fn plan_repair(recorded: &[RecMic], snap: &[Snap]) -> Vec<Step> {
        let mut taken: Vec<String> = recorded.iter().map(|r| r.id.clone()).collect();
        let mut steps = Vec::new();
        for rec in recorded {
            let current = snap.iter().find(|s| same_id(&s.id, &rec.id));
            if let Some(s) = current.filter(|s| s.active) {
                steps.push(if s.has_hush {
                    Step::Ok { id: s.id.clone(), name: s.name.clone() }
                } else {
                    Step::Rewrite {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        slot: rec.slot.clone(),
                    }
                });
                continue;
            }
            let name = current
                .map(|s| s.name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| rec.label.clone())
                .unwrap_or_default();
            if name.is_empty() {
                steps.push(Step::Waiting { name, ambiguous: false });
                continue;
            }
            let candidates: Vec<&Snap> = snap
                .iter()
                .filter(|s| {
                    s.active
                        && s.name.eq_ignore_ascii_case(&name)
                        && !taken.iter().any(|t| same_id(t, &s.id))
                })
                .collect();
            let with_hush: Vec<&&Snap> = candidates.iter().filter(|s| s.has_hush).collect();
            let without: Vec<&&Snap> = candidates.iter().filter(|s| !s.has_hush).collect();
            let pool = if with_hush.is_empty() { without } else { with_hush };
            match pool.len() {
                0 => steps.push(Step::Waiting { name, ambiguous: false }),
                1 => {
                    let new = pool[0].id.clone();
                    taken.push(new.clone());
                    steps.push(Step::Migrate {
                        old: rec.id.clone(),
                        new,
                        name,
                        slot: rec.slot.clone(),
                    });
                }
                _ => steps.push(Step::Waiting { name, ambiguous: true }),
            }
        }
        steps
    }

    /// Pure summary: (result, message). `restarted` = audio restarted without
    /// any registry change.
    fn summarize(steps: &[Step], restarted: bool) -> (&'static str, String) {
        if steps.is_empty() {
            return ("ok", "No microphone is set up with Clean Mic.".into());
        }
        let waiting_text = |step: &Step| match step {
            Step::Waiting { name, ambiguous: true } => Some(format!(
                "Several microphones are named {name}; open Hush and choose one."
            )),
            Step::Waiting { name, .. } if name.is_empty() => {
                Some("Waiting for the microphone to be connected.".to_string())
            }
            Step::Waiting { name, .. } => Some(format!("Waiting for {name}.")),
            _ => None,
        };
        let restored: Vec<String> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Rewrite { name, .. } | Step::Migrate { name, .. } => {
                    Some(format!("Clean Mic restored on {name}."))
                }
                _ => None,
            })
            .collect();
        if !restored.is_empty() {
            let mut parts = restored;
            parts.extend(steps.iter().filter_map(waiting_text));
            return ("repaired", parts.join(" "));
        }
        let active: Vec<&str> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Ok { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        if restarted && !active.is_empty() {
            return (
                "restarted",
                format!("Windows audio restarted; Clean Mic reloaded on {}.", active.join(", ")),
            );
        }
        if !active.is_empty() {
            return ("ok", format!("Clean Mic is attached to {}.", active.join(", ")));
        }
        let message = steps.iter().filter_map(waiting_text).collect::<Vec<_>>().join(" ");
        ("waiting", message)
    }

    struct RepairReport {
        time_unix: u64,
        result: String,
        message: String,
        endpoint: String,
    }

    fn render_repair(report: &RepairReport) -> String {
        let one_line = |s: &str| s.replace(['\r', '\n'], " ");
        format!(
            "time_unix = {}\nresult = {}\nmessage = {}\nendpoint = {}\n",
            report.time_unix,
            one_line(&report.result),
            one_line(&report.message),
            one_line(&report.endpoint)
        )
    }

    fn parse_repair(text: &str) -> Option<RepairReport> {
        let (mut time_unix, mut result, mut message, mut endpoint) =
            (None, None, String::new(), String::new());
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            match key.trim() {
                "time_unix" => time_unix = value.parse().ok(),
                "result" => result = Some(value),
                "message" => message = value,
                "endpoint" => endpoint = value,
                _ => {}
            }
        }
        Some(RepairReport {
            time_unix: time_unix?,
            result: result?,
            message,
            endpoint,
        })
    }

    fn repair_path() -> PathBuf {
        settings::settings_dir().join(REPAIR_FILE)
    }

    fn read_repair_file() -> Option<RepairReport> {
        parse_repair(&fs::read_to_string(repair_path()).ok()?)
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Atomic (temp + rename). The file inherits the directory's ACL, which
    /// lets standard users read it.
    fn write_repair_file(report: &RepairReport) -> Result<()> {
        use std::io::Write;
        fs::create_dir_all(settings::settings_dir())?;
        let text = render_repair(report);
        replace_file(&repair_path(), |file| file.write_all(text.as_bytes()))
            .context("writing repair.txt")
    }

    /// True when Hush's value is present in any slot (and, for the endpoint
    /// effect slot, its processing modes too).
    fn hush_present(flow: Flow, id: &str) -> bool {
        let Ok(key) = hklm().open_subkey_with_flags(fx_path(flow, id), KEY_READ) else {
            return false;
        };
        let modes: Vec<String> = MODES.iter().map(|m| m.to_string()).collect();
        let want = modes.to_reg_value();
        for slot in [1, 2, 7] {
            let name = format!("{FX},{slot}");
            let ours = matches!(
                raw_value(&key, &name),
                Ok(Some(v)) if v.to_string().eq_ignore_ascii_case(CLSID_STRING)
            );
            if !ours {
                continue;
            }
            if slot == 7
                && raw_value(&key, &format!("{FX_MODES},7"))
                    .ok()
                    .flatten()
                    .as_ref()
                    != Some(&want)
            {
                continue;
            }
            return true;
        }
        false
    }

    /// Capture endpoints from the install record, one per endpoint id.
    fn recorded_mics() -> Vec<RecMic> {
        let mut mics: Vec<RecMic> = Vec::new();
        for line in read_record() {
            if line.flow != Flow::Capture {
                continue;
            }
            let slot = if line.value == format!("{FX},1") {
                Some("lfx")
            } else if line.value == format!("{FX},2") {
                Some("gfx")
            } else if line.value == format!("{FX},7") {
                Some("efx")
            } else {
                None // the processing-modes value
            };
            match mics.iter_mut().find(|m| same_id(&m.id, &line.id)) {
                Some(mic) => {
                    if let Some(slot) = slot {
                        mic.slot = slot.into();
                    }
                    if mic.label.is_none() {
                        mic.label = line.label;
                    }
                }
                None => mics.push(RecMic {
                    id: line.id,
                    slot: slot.unwrap_or("efx").into(),
                    label: line.label,
                }),
            }
        }
        mics
    }

    fn repair_inner(out: &Out, restart_audio: bool) -> Result<RepairReport> {
        let list = endpoints(Flow::Capture)?;
        let snap: Vec<Snap> = list
            .iter()
            .map(|e| Snap {
                id: e.id.clone(),
                name: e.name.clone(),
                active: e.active,
                has_hush: e.active && hush_present(Flow::Capture, &e.id),
            })
            .collect();
        let steps = plan_repair(&recorded_mics(), &snap);
        let changed = steps.iter().any(Step::changes);
        let any_active = steps.iter().any(|s| matches!(s, Step::Ok { .. }));
        let restart = changed || (restart_audio && any_active);
        if restart {
            let find = |id: &str| {
                list.iter()
                    .find(|e| same_id(&e.id, id))
                    .ok_or_else(|| anyhow!("microphone {id} disappeared during repair"))
            };
            with_audio_restart(out, true, || {
                for step in &steps {
                    match step {
                        Step::Rewrite { id, slot, .. } => {
                            install_endpoint(out, find(id)?, slot, false, false, &[])?;
                        }
                        Step::Migrate { old, new, slot, .. } => {
                            install_endpoint(
                                out,
                                find(new)?,
                                slot,
                                false,
                                false,
                                std::slice::from_ref(old),
                            )?;
                        }
                        _ => {}
                    }
                }
                Ok(())
            })?;
        }
        let (result, message) = summarize(&steps, restart && !changed);
        let endpoint = steps
            .iter()
            .find_map(|s| match s {
                Step::Ok { id, .. } | Step::Rewrite { id, .. } => Some(id.clone()),
                Step::Migrate { new, .. } => Some(new.clone()),
                Step::Waiting { .. } => None,
            })
            .unwrap_or_default();
        Ok(RepairReport {
            time_unix: now_unix(),
            result: result.into(),
            message,
            endpoint,
        })
    }

    fn repair(out: &Out, restart_audio: bool) -> Result<()> {
        let quiet = Out {
            log: out.log.clone(),
            mute: true,
        };
        // Existing installs get the on-demand tasks on their first repair.
        if let Err(error) = create_repair_tasks(&quiet) {
            quiet.line(format!("warning: could not create the repair tasks: {error:#}"));
        }
        match repair_inner(&quiet, restart_audio) {
            Ok(report) => {
                if let Err(error) = write_repair_file(&report) {
                    quiet.line(format!("warning: {error:#}"));
                }
                out.line(format!("repair: {}: {}", report.result, report.message));
                Ok(())
            }
            Err(error) => {
                let _ = write_repair_file(&RepairReport {
                    time_unix: now_unix(),
                    result: "error".into(),
                    message: format!("Repair failed: {error:#}"),
                    endpoint: String::new(),
                });
                Err(error)
            }
        }
    }

    // ---- scheduled tasks ----------------------------------------------------

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn task_xml(exe: &str, arguments: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Hush</Author>
    <Description>Re-attaches the Hush Clean Mic effect to the microphone. Started on demand by Hush.</Description>
    <SecurityDescriptor>{sddl}</SecurityDescriptor>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT2M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>"{exe}"</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
            sddl = TASK_SDDL,
            exe = xml_escape(exe),
            arguments = xml_escape(arguments)
        )
    }

    fn schtasks() -> Result<std::process::Command> {
        let mut command = system_tool("schtasks.exe")?;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        Ok(command)
    }

    /// Whether this is `%ProgramFiles%\Hush\hush-setup.exe`, the only copy the
    /// repair tasks may point at.
    fn is_installed_copy() -> bool {
        let (Ok(dir), Ok(current)) = (program_dir(), std::env::current_exe()) else {
            return false;
        };
        let exe = dir.join(SETUP_EXE);
        matches!(
            (fs::canonicalize(&exe), fs::canonicalize(&current)),
            (Ok(a), Ok(b)) if a == b
        )
    }

    /// Creates (or replaces) both tasks. Refuses unless this exe is the one in
    /// the Program Files folder, because the tasks run it as SYSTEM and only
    /// an administrator-writable location is safe for that.
    fn create_repair_tasks(out: &Out) -> Result<()> {
        use std::io::Write;
        let dir = program_dir()?;
        let exe = dir.join(SETUP_EXE);
        if !is_installed_copy() {
            bail!(
                "hush-setup is not running from {} (repair tasks are only created by the installed copy)",
                dir.display()
            );
        }
        for (name, arguments) in REPAIR_TASKS {
            let xml = task_xml(&exe.display().to_string(), arguments);
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
            // In the admin-only program folder so nobody can swap the file.
            let temp = dir.join(format!("repair-task.{}.xml", std::process::id()));
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp)
                .with_context(|| format!("creating {}", temp.display()))?;
            let written = file.write_all(&bytes).and_then(|()| file.sync_all());
            drop(file);
            let result = written.map_err(anyhow::Error::from).and_then(|()| {
                let output = schtasks()?
                    .args(["/create", "/tn", name, "/xml"])
                    .arg(&temp)
                    .arg("/f")
                    .output()
                    .context("running schtasks")?;
                if output.status.success() {
                    Ok(())
                } else {
                    bail!(
                        "schtasks /create {name} failed: {}{}",
                        String::from_utf8_lossy(&output.stdout).trim(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                }
            });
            let _ = fs::remove_file(&temp);
            result?;
            out.line(format!("  scheduled task {name}"));
        }
        Ok(())
    }

    /// Removes both tasks; a task that does not exist is fine.
    fn delete_repair_tasks(out: &Out) {
        for (name, _) in REPAIR_TASKS {
            let Ok(mut command) = schtasks() else { continue };
            let removed = command
                .args(["/delete", "/tn", name, "/f"])
                .output()
                .is_ok_and(|o| o.status.success());
            if removed {
                out.line(format!("  removed scheduled task {name}"));
            }
        }
    }

    fn task_exists(name: &str) -> bool {
        schtasks()
            .and_then(|mut c| c.args(["/query", "/tn", name]).output().map_err(Into::into))
            .is_ok_and(|o| o.status.success())
    }

    fn repair_task_status() {
        for (name, _) in REPAIR_TASKS {
            println!(
                "Repair task {name}: {}",
                if task_exists(name) { "present" } else { "missing" }
            );
        }
    }

    /// Registry key for Windows' "Installed apps" entry (written by the installer).
    pub const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Hush";

    fn start_menu_shortcut() -> PathBuf {
        std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join(r"Microsoft\Windows\Start Menu\Programs\Hush.lnk")
    }

    /// Removes everything the Hush installer added: the APO and its
    /// settings, the running app, Start menu shortcut, autostart entry,
    /// "Installed apps" entry and the program folder.
    fn uninstall_app(out: &Out, quiet: bool) -> Result<()> {
        out.line("Uninstalling Hush");
        let _ = system_tool("taskkill.exe").and_then(|mut command| {
            command
                .args(["/im", "hush.exe", "/f"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        });
        uninstall(out, true, true)?;

        let shortcut = start_menu_shortcut();
        if fs::remove_file(&shortcut).is_ok() {
            out.line(format!("  removed {}", shortcut.display()));
        }
        // Per-user parts (this process runs elevated as the same user).
        if let Ok(run) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey_with_flags(
                r"Software\Microsoft\Windows\CurrentVersion\Run",
                KEY_READ | KEY_SET_VALUE,
            )
        {
            if run.delete_value("Hush").is_ok() {
                out.line("  removed Start with Windows entry");
            }
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let _ = fs::remove_dir_all(PathBuf::from(appdata).join("Hush"));
        }
        if hklm().delete_subkey_all(UNINSTALL_KEY).is_ok() {
            out.line("  removed Installed apps entry");
        }

        // This exe lives in the program folder, so delete the folder after
        // it exits.
        let dir = program_dir()?;
        let _ = system_tool("cmd.exe")?
            .args([
                "/d",
                "/c",
                &format!(
                    "ping 127.0.0.1 -n 3 > nul & rmdir /s /q \"{}\"",
                    dir.display()
                ),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        out.line(format!("  removing {}", dir.display()));
        out.line("Hush has been removed.");
        if !quiet {
            message_box("Hush has been removed from this PC.");
        }
        Ok(())
    }

    fn message_box(text: &str) {
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};
        let text = windows_core::HSTRING::from(text);
        unsafe {
            MessageBoxW(
                None,
                &text,
                windows_core::w!("Hush"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }

    pub fn run() -> Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let value_of = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        let has = |flag: &str| args.iter().any(|a| a == flag);
        let out = Out {
            log: value_of("--log").map(PathBuf::from),
            mute: false,
        };
        let command = args.first().map(String::as_str).unwrap_or("help");
        let needs_admin = matches!(
            command,
            "install" | "uninstall" | "uninstall-app" | "repair" | "create-repair-tasks"
        );
        if needs_admin && !is_admin() {
            let forwarded: Vec<String> = args.iter().filter(|a| !a.is_empty()).cloned().collect();
            return relaunch_elevated(&forwarded);
        }
        let restart = !has("--no-restart");
        let result = match command {
            "list" => list(),
            "status" => status(),
            "install" => {
                let mic = value_of("--mic").ok_or_else(|| anyhow!("install needs --mic <name>"))?;
                let slot = value_of("--slot").unwrap_or_else(|| "efx".into());
                install(&out, &mic, &slot, restart, has("--replace-mic"))
            }
            "repair" => repair(&out, has("--restart-audio")),
            "repair-task-status" => {
                repair_task_status();
                Ok(())
            }
            "create-repair-tasks" => create_repair_tasks(&out).map(|()| {
                out.line("Repair tasks created.");
            }),
            "uninstall" => uninstall(&out, restart, has("--purge")),
            "uninstall-app" => uninstall_app(&out, has("--quiet")),
            _ => {
                println!(
                    "usage: hush-setup list | status | install --mic <name> [--slot efx|lfx] | repair [--restart-audio] [--quiet] | repair-task-status | create-repair-tasks | uninstall [--purge] | uninstall-app [--quiet]"
                );
                Ok(())
            }
        };
        if let Err(error) = &result {
            out.line(format!("error: {error:#}"));
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn rec(id: &str) -> RecMic {
            RecMic { id: id.into(), slot: "efx".into(), label: None }
        }
        fn snap(id: &str, name: &str, active: bool, has_hush: bool) -> Snap {
            Snap { id: id.into(), name: name.into(), active, has_hush }
        }

        #[test]
        fn unchanged_when_active_with_hush() {
            let steps = plan_repair(&[rec("a")], &[snap("a", "Mic", true, true)]);
            assert_eq!(steps, vec![Step::Ok { id: "a".into(), name: "Mic".into() }]);
            assert!(!steps[0].changes());
            assert_eq!(summarize(&steps, false).0, "ok");
            assert_eq!(summarize(&steps, true).0, "restarted");
        }

        #[test]
        fn rewrites_when_values_missing() {
            let steps = plan_repair(&[rec("a")], &[snap("a", "Mic", true, false)]);
            assert!(matches!(&steps[0], Step::Rewrite { id, .. } if id == "a"));
            let (result, message) = summarize(&steps, false);
            assert_eq!(result, "repaired");
            assert_eq!(message, "Clean Mic restored on Mic.");
        }

        #[test]
        fn migrates_to_new_id_with_same_name() {
            let snaps = [
                snap("old", "Mic", false, true),
                snap("new", "Mic", true, false),
                snap("other", "Webcam", true, false),
            ];
            let steps = plan_repair(&[rec("old")], &snaps);
            assert!(
                matches!(&steps[0], Step::Migrate { old, new, .. } if old == "old" && new == "new")
            );
            assert_eq!(summarize(&steps, false).0, "repaired");
        }

        #[test]
        fn waits_when_unplugged_or_deleted() {
            let steps = plan_repair(&[rec("old")], &[snap("old", "Mic", false, true)]);
            assert_eq!(steps, vec![Step::Waiting { name: "Mic".into(), ambiguous: false }]);
            assert_eq!(summarize(&steps, true), ("waiting", "Waiting for Mic.".to_string()));
            // Key gone entirely: the recorded label still names it.
            let labelled = RecMic { label: Some("Mic".into()), ..rec("gone") };
            let steps = plan_repair(&[labelled], &[snap("x", "Webcam", true, false)]);
            assert!(matches!(&steps[0], Step::Waiting { name, .. } if name == "Mic"));
            // Nothing known about it at all.
            let steps = plan_repair(&[rec("gone")], &[]);
            assert_eq!(summarize(&steps, false).0, "waiting");
        }

        #[test]
        fn ambiguous_candidates_are_not_guessed() {
            let snaps = [
                snap("old", "Mic", false, false),
                snap("n1", "Mic", true, false),
                snap("n2", "Mic", true, false),
            ];
            let steps = plan_repair(&[rec("old")], &snaps);
            assert_eq!(steps, vec![Step::Waiting { name: "Mic".into(), ambiguous: true }]);
            let (result, message) = summarize(&steps, false);
            assert_eq!(result, "waiting");
            assert!(message.contains("Several microphones"));
        }

        #[test]
        fn candidate_already_holding_hush_is_preferred() {
            let snaps = [
                snap("old", "Mic", false, false),
                snap("n1", "Mic", true, false),
                snap("n2", "Mic", true, true),
            ];
            let steps = plan_repair(&[rec("old")], &snaps);
            assert!(matches!(&steps[0], Step::Migrate { new, .. } if new == "n2"));
        }

        #[test]
        fn record_roundtrip_and_old_format() {
            let text = format!(
                "Capture\ta\t{FX},7\tMic (x)\nCapture\ta\t{FX_MODES},7\tMic (x)\nCapture\tb\t{FX},1\n"
            );
            let lines = parse_record(&text);
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0].label.as_deref(), Some("Mic (x)"));
            assert_eq!(lines[2].label, None);
            let rendered: String = lines.iter().map(RecLine::render).collect();
            assert_eq!(rendered, text);
        }

        #[test]
        fn repair_file_roundtrip() {
            let report = RepairReport {
                time_unix: 42,
                result: "repaired".into(),
                message: "Clean Mic restored on Mic = x.\nsecond".into(),
                endpoint: "{0.0.1.00000000}.{abc}".into(),
            };
            let parsed = parse_repair(&render_repair(&report)).unwrap();
            assert_eq!(parsed.time_unix, 42);
            assert_eq!(parsed.result, "repaired");
            assert_eq!(parsed.message, "Clean Mic restored on Mic = x. second");
            assert_eq!(parsed.endpoint, report.endpoint);
        }

        #[test]
        fn task_xml_has_required_settings() {
            let xml = task_xml(r"C:\Program Files\Hush\hush-setup.exe", "repair --restart-audio --quiet");
            for needle in [
                "<SecurityDescriptor>D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;GRGX;;;AU)</SecurityDescriptor>",
                "<UserId>S-1-5-18</UserId>",
                "<RunLevel>HighestAvailable</RunLevel>",
                "<Triggers />",
                "<AllowStartOnDemand>true</AllowStartOnDemand>",
                "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
                "<ExecutionTimeLimit>PT2M</ExecutionTimeLimit>",
                "<Hidden>true</Hidden>",
                r#"<Command>"C:\Program Files\Hush\hush-setup.exe"</Command>"#,
                "<Arguments>repair --restart-audio --quiet</Arguments>",
            ] {
                assert!(xml.contains(needle), "missing {needle}");
            }
        }
    }
}
