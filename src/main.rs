//! Broforce 修改器 —— 一键把生命改成 999。
//!
//! 界面用 Slint（`ui/app.slint`），业务逻辑在 `trainer.rs`：
//! 在游戏进程里借 mono 的只读 API 定位 `Player.lives`，然后由本进程写那 4 个字节。
//! 为什么不直接调游戏的 `SetLives`，见 `trainer.rs` 顶部的说明。
//!
//! 用法：
//!   1. 启动 Broforce，**进入关卡**（玩家对象不存在时改不了）；
//!   2. 运行本程序，点「连接」；
//!   3. 点「设为 999 生命」。

// release 构建时不要额外弹一个控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod inject;
mod mono;
mod pe;
mod trainer;
mod win32;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    app::TrainerApp::new()?.run()
}
