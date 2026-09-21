fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rerun-if-changed=src/macos_process.c");
        cc::Build::new()
            .file("src/macos_process.c")
            .compile("cx_macos_process");
        println!("cargo:rustc-link-lib=proc");
    }
}
