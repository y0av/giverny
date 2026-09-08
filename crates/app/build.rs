//! Embeds the application icon into the Windows executable.
//!
//! The window icon is set at runtime from the same artwork; this is the other
//! one — what Explorer, the taskbar and a Start-menu shortcut show, which
//! Windows reads from a resource inside the `.exe` and nowhere else. Without
//! it they show the generic "unknown program" icon, which is what a Windows
//! user sees before ever opening the app.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/icon/giverny.ico");
    println!("cargo:rerun-if-changed={icon}");
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(icon);
    // Cross-building without a resource compiler is not a reason to fail the
    // build; it costs the icon, not the binary.
    if let Err(err) = resource.compile() {
        println!("cargo:warning=icon not embedded: {err}");
    }
}
