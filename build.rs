// Embed the icon and version info into the Windows executables.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set("ProductName", "RDS FM Monitor")
            .set("FileDescription", "RDS FM Monitor — SDR radio, RDS decoder and streamer")
            .set("LegalCopyright", "Kittiwut Khongkaeo");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon not embedded: {e}");
        }
    }
}
