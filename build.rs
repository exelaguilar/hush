// Embeds the Hush icon and version information into the Windows
// executables (shown in Explorer, the taskbar and Apps & features).

include!("src/icon_art.rs");

fn main() {
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("hush.ico");
    std::fs::write(&out, build_ico(&[16, 20, 24, 32, 40, 48, 64, 256])).unwrap();
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(out.to_str().unwrap())
        .set("ProductName", "Hush")
        .set("FileDescription", "Hush")
        .set("CompanyName", "Exel")
        .set("LegalCopyright", "Copyright (c) 2026 Exel");
    if let Err(error) = resource.compile() {
        println!("cargo:warning=could not embed the Windows icon resource: {error}");
    }
}
