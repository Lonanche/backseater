/// Embeds the Windows icon resource. gpui's Windows platform loads the window/
/// taskbar icon from the exe's icon resource with ordinal **1** (`load_icon` in
/// gpui_windows), so the id must stay "1"; Explorer shows the same icon for the
/// exe file. `resources/icon.ico` is generated from `resources/icon.png`.
fn main() {
    println!("cargo:rerun-if-changed=resources/icon.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        winresource::WindowsResource::new()
            .set_icon_with_id("resources/icon.ico", "1")
            .compile()
            .expect("failed to embed the Windows icon resource");
    }
}
