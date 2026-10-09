//! 构建脚本:把「PawnIO 安装器是否存在」变成一个编译期开关。
//!
//! `assets/PawnIO_setup.exe` 是一个 3.4 MB 的第三方签名可执行文件,不随仓库
//! 分发(由 `tool/fetch_assets.ps1` 按需下载并校验)。但 `driver/setup.rs`
//! 想把它内嵌进二进制,让发布形态零外部依赖。
//!
//! 直接用 `include_bytes!` 不行:克隆仓库后没跑 fetch_assets 就 `cargo build`
//! 会**编译失败**。缺一个可选资源不该阻止别人编译这个库。所以这里在构建时
//! 探测文件:
//!
//! - 存在 → 设置 `hwprobe_embedded_pawnio_setup`,`setup.rs` 内嵌它;
//! - 不存在 → 不设置该 cfg,`setup.rs` 改为运行期在磁盘上找安装器,
//!   找不到时给出一条可操作的提示。
//!
//! 探测到的存在性同时作为 `cargo:rerun-if-changed` 的输入,所以先构建再
//! 下拉安装器也能触发重新构建、把资源嵌进去。

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Declare the custom cfg so rustc's `unexpected_cfgs` lint stays quiet.
    println!("cargo:rustc-check-cfg=cfg(hwprobe_embedded_pawnio_setup)");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let installer = Path::new(&manifest_dir).join("assets").join("PawnIO_setup.exe");

    // 文件增删都要触发重新构建,所以监听目录本身而不仅是文件。
    println!("cargo:rerun-if-changed=assets");

    if installer.is_file() {
        println!("cargo:rustc-cfg=hwprobe_embedded_pawnio_setup");
        // 安装器更新时要重新内嵌。
        println!("cargo:rerun-if-changed=assets/PawnIO_setup.exe");
        println!(
            "cargo:warning=PawnIO_setup.exe 已内嵌({} 字节);发布产物将自带一键装驱动能力。",
            std::fs::metadata(&installer).map(|m| m.len()).unwrap_or(0)
        );
    } else {
        println!(
            "cargo:warning=未找到 assets/PawnIO_setup.exe —— 将不内嵌安装器。\
             库照常构建,hwprobe_setup_driver() 会在运行期到磁盘上寻找安装器。\
             运行 `powershell -ExecutionPolicy Bypass -File tool/fetch_assets.ps1` \
             下载并校验后再构建即可内嵌。"
        );
    }
}
