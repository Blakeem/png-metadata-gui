//! Embeds the app icon into the Windows executable resource section, so
//! Explorer, the taskbar, and Alt-Tab show it. Other targets have no
//! resource section — the runtime window icon in `main.rs` covers them.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        _ = resource.set_icon("assets/icon.ico");
        resource
            .compile()
            .expect("embedding assets/icon.ico via rc.exe failed");
    }
}
