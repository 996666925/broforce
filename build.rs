//! 把 `ui/app.slint` 编译进二进制。
//!
//! 界面文件改动之后 cargo 会自动重跑这个脚本（`slint-build` 会自己声明
//! `cargo:rerun-if-changed`），不用手动 `touch` 任何东西。

fn main() {
    slint_build::compile("ui/app.slint").expect("编译 Slint 界面失败");
}
