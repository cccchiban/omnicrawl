//! 把 npm 产品版本注入编译期常量。
//!
//! 产品版本只有一处事实来源：`packages/cli/package.json`。Cargo workspace 自己的版本
//! 与它并不同步（发布按 npm 版本走），所以不能拿 `CARGO_PKG_VERSION` 顶替。
//! 读不到那份文件时回落包版本，源码直接 `cargo build` 不会因此失败。

use std::ffi::OsString;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let package = manifest.join("../../../packages/cli/package.json");
    println!("cargo:rerun-if-changed={}", package.display());

    let version = product_version(&package).unwrap_or_else(|| {
        std::env::var_os("CARGO_PKG_VERSION")
            .unwrap_or_else(|| OsString::from("0.0.0"))
            .to_string_lossy()
            .to_string()
    });
    println!("cargo:rustc-env=OMNICRAWL_VERSION={version}");
}

fn product_version(path: &PathBuf) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    let version = manifest.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}
