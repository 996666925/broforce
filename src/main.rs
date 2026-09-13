//! Broforce 修改器 —— 一键把生命改成 999。
//!
//! 原理：Broforce 是 Unity + Mono 构建的，`mono.dll` 导出了完整的 C API。
//! 于是在游戏进程里跑一小段手写的 x64 代码，通过 mono API 找到并调用
//! `HeroController.SetLives(playerNum, 999)`，也就是游戏自己那个设生命的方法。
//! 这样不必猜任何内存布局，游戏更新后也不容易失效。
//!
//! 用法：
//!   1. 启动 Broforce，**进入关卡**（玩家对象不存在时改不了）；
//!   2. 运行本程序，点「附加」；
//!   3. 点「设为 999 生命」。

// release 构建时不要额外弹一个控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod inject;
mod mono;
mod pe;
mod trainer;
mod win32;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Broforce 修改器 — 999 生命")
            .with_inner_size([480.0, 740.0])
            .with_min_inner_size([420.0, 520.0])
            // 游戏多半是全屏，窗口置顶才点得到。
            .with_always_on_top(),
        ..Default::default()
    };

    eframe::run_native(
        "Broforce 修改器",
        options,
        Box::new(|cc| Ok(Box::new(app::TrainerApp::new(cc)))),
    )
}
