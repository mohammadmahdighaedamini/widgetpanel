fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // app icon + version info inside the Windows .exe (checks the *target*, not the machine building it)
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Widget Panel");
        res.set("FileDescription", "Widget Panel");
        res.set("CompanyName", "Nightmare");
        res.set("LegalCopyright", "Developed by Nightmare");
        res.compile().expect("Failed to compile Windows resources");
    }
}