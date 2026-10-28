//! Embeds the app icon and version info into the Windows executables.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Bonsai H3 Writer");
        res.set("FileDescription", "Bonsai H3 Writer - MiniMax H3 prompt writer for a local Bonsai 27B");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon/version resource not embedded: {e}");
        }
    }
}
