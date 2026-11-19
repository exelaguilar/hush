// Embeds the release build of Hush into the installer and gives the
// installer its icon and an "administrator" manifest (UAC prompt on launch).
//
// Build with package.ps1 (it builds the payload first). SM_PAYLOAD_DIR
// overrides where the payload is taken from (default: ../target/release).

include!("../src/icon_art.rs");

const FILES: [&str; 3] = ["hush.exe", "hush-setup.exe", "hush_apo.dll"];

/// Writes `[original length: u64 LE][brotli stream]`. Quality 11 with a
/// 16 MiB window is slow but only runs when the payload changes; unpacking
/// takes well under a second.
fn compress(from: &std::path::Path, to: &std::path::Path) {
    use std::io::Write;
    let data = std::fs::read(from).unwrap_or_else(|e| panic!("reading {}: {e}", from.display()));
    let mut out = (data.len() as u64).to_le_bytes().to_vec();
    {
        let mut writer = brotli::CompressorWriter::new(&mut out, 1 << 16, 11, 24);
        writer.write_all(&data).unwrap();
    }
    std::fs::write(to, out).unwrap();
}

fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let payload = std::env::var_os("SM_PAYLOAD_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("..").join("target").join("release"));
    let payload = payload.canonicalize().unwrap_or(payload);
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for file in FILES {
        let path = payload.join(file);
        if !path.exists() {
            panic!(
                "{} is missing. Build the payload first: run package.ps1 (or cargo build --release --workspace).",
                path.display()
            );
        }
        println!("cargo:rerun-if-changed={}", path.display());
        compress(&path, &out_dir.join(format!("{file}.br")));
    }
    println!("cargo:rerun-if-env-changed=SM_PAYLOAD_DIR");
    println!("cargo:rerun-if-changed=../src/icon_art.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = out_dir.join("setup.ico");
    std::fs::write(&out, build_ico(&[16, 20, 24, 32, 40, 48, 64, 256])).unwrap();
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(out.to_str().unwrap())
        .set("ProductName", "Hush")
        .set("FileDescription", "Hush Setup")
        .set("CompanyName", "Exel")
        .set_manifest(
            r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
        );
    resource.compile().expect("embedding installer resources");
}
