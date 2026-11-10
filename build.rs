fn main() {
    println!("cargo:rerun-if-changed=Assets/AppIcon.ico");
    println!("cargo:rerun-if-changed=app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("Assets/AppIcon.ico")
            .set_manifest_file("app.manifest")
            .set("ProductName", "CFUnpacker")
            .set("FileDescription", "CFUnpacker — 熔萤FluorescentLava")
            .compile()
            .expect("embed application icon and Windows manifest");
    }
}
