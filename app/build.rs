fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rerun-if-changed=assets/zh3.ico");
        winresource::WindowsResource::new()
            .set_icon_with_id("assets/zh3.ico", "101")
            .set("ProductName", "ZH3 Control")
            .set(
                "FileDescription",
                "ZH3 Control · 第三方 WASAPI 共享模式配置工具",
            )
            .set("OriginalFilename", "ZH3-Control.exe")
            .compile()
            .expect("compile Windows icon and version resources");
        println!("cargo:rustc-link-arg-bin=ZH3-Control=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=ZH3-Control=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );
    }
}
