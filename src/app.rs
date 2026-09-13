//! egui 界面。
//!
//! 布局是「两张卡片 + 一份日志」：先连接，再改生命，日志全程留痕。
//! 日志按级别着色——这个项目排查问题时吃过亏，失败信息混在一堆灰字里
//! 等于没有，所以错误一律标红，还要能一键复制出来。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use crate::trainer::{Report, Steps, Trainer};
use crate::win32;

/// 默认要找的可执行文件名。Steam 现版本用的是 `Broforce_beta.exe`。
const DEFAULT_EXE: &str = "Broforce_beta.exe";
/// 备用名，免得用户还得自己去改。
const FALLBACK_EXE: &str = "Broforce.exe";
/// 已经拿到地址之后，刷新「当前生命」的间隔。只是读内存，很便宜。
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// 配色：深灰蓝底 + 琥珀强调，配合游戏的硬汉调性。
mod palette {
    use egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x12, 0x14, 0x18);
    pub const CARD: Color32 = Color32::from_rgb(0x1A, 0x1E, 0x25);
    pub const BORDER: Color32 = Color32::from_rgb(0x2B, 0x31, 0x3C);

    pub const TEXT: Color32 = Color32::from_rgb(0xDD, 0xE1, 0xE6);
    pub const DIM: Color32 = Color32::from_rgb(0x86, 0x8D, 0x98);

    pub const ACCENT: Color32 = Color32::from_rgb(0xE9, 0xA5, 0x3B);
    pub const OK: Color32 = Color32::from_rgb(0x5C, 0xC9, 0x7A);
    pub const ERR: Color32 = Color32::from_rgb(0xE3, 0x6C, 0x6C);
}

/// 日志级别，决定颜色和行首标记。
#[derive(Clone, Copy)]
enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

struct LogLine {
    level: Level,
    text: String,
}

impl LogLine {
    fn new(level: Level, text: impl Into<String>) -> Self {
        Self {
            level,
            text: text.into(),
        }
    }

    fn mark(&self) -> &'static str {
        match self.level {
            Level::Info => "·",
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Error => "✕",
        }
    }

    fn color(&self) -> egui::Color32 {
        match self.level {
            Level::Info => palette::DIM,
            Level::Ok => palette::OK,
            Level::Warn => palette::ACCENT,
            Level::Error => palette::ERR,
        }
    }
}

/// 界面上的一次点击，收集起来在绘制结束后统一执行。
enum Action {
    Attach,
    Detach,
    Apply,
    Detect,
    ClearLog,
    CopyLog,
}

/// 一次只读检测的结果。
enum DetectOutcome {
    Found {
        addr: usize,
        value: Option<i32>,
        steps: Steps,
    },
    NotFound {
        error: String,
        steps: Steps,
    },
    Panic(String),
}

/// 后台线程做完一件事之后回传的结果。
enum Outcome {
    Set(Report),
    Detect(DetectOutcome),
}

/// 操作结束后显示在按钮下方的醒目提示。
enum Banner {
    None,
    Ok(String),
    Err(String),
}

pub struct TrainerApp {
    exe_name: String,
    attached: bool,
    pid: Option<u32>,

    trainer: Option<Arc<Trainer>>,
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
    busy: bool,
    banner: Banner,

    player: String,
    lives: String,
    /// 已经定位到的生命值地址；拿到之后只做普通内存读，不再建远程线程。
    lives_addr: Option<usize>,
    lives_now: Option<i32>,
    last_poll: Instant,
    force_poll: bool,

    log: Vec<LogLine>,
}

impl TrainerApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        apply_theme(&cc.egui_ctx);
        let (tx, rx) = channel();

        Self {
            exe_name: DEFAULT_EXE.to_owned(),
            attached: false,
            pid: None,
            trainer: None,
            tx,
            rx,
            busy: false,
            banner: Banner::None,
            player: "0".to_owned(),
            lives: "999".to_owned(),
            lives_addr: None,
            lives_now: None,
            last_poll: Instant::now(),
            force_poll: false,
            log: vec![
                LogLine::new(
                    Level::Info,
                    "用法：启动 Broforce 并进入关卡 → 连接 → 点「设为 999 生命」。",
                ),
                LogLine::new(Level::Warn, "仅限单人模式，联机使用会影响其他玩家。"),
            ],
        }
    }

    fn log(&mut self, level: Level, msg: impl Into<String>) {
        self.log.push(LogLine::new(level, msg));
        if self.log.len() > 400 {
            self.log.drain(..self.log.len() - 400);
        }
    }

    fn info(&mut self, msg: impl Into<String>) {
        self.log(Level::Info, msg);
    }

    fn clear_located(&mut self) {
        self.lives_addr = None;
        self.lives_now = None;
        self.banner = Banner::None;
    }

    fn parse_player(&mut self) -> Option<i32> {
        match self.player.trim().parse::<i32>() {
            Ok(v) if (0..4).contains(&v) => Some(v),
            _ => {
                self.log(Level::Error, "玩家序号要填 0 到 3 之间的整数。");
                None
            }
        }
    }

    fn parse_lives(&mut self) -> Option<i32> {
        match self.lives.trim().parse::<i32>() {
            Ok(v) => Some(v),
            Err(_) => {
                self.log(Level::Error, "生命值要填整数。");
                None
            }
        }
    }

    // ---------- 连接 ----------

    fn attach(&mut self) {
        let wanted = self.exe_name.trim().to_owned();
        let mut pids = win32::find_pids(&wanted);

        if pids.is_empty() && !wanted.eq_ignore_ascii_case(FALLBACK_EXE) {
            pids = win32::find_pids(FALLBACK_EXE);
            if !pids.is_empty() {
                self.info(format!("没找到 {wanted}，改用 {FALLBACK_EXE}"));
                self.exe_name = FALLBACK_EXE.to_owned();
            }
        }

        let Some(&pid) = pids.first() else {
            self.log(
                Level::Error,
                format!("没有找到进程 {wanted}，请先启动游戏。"),
            );
            return;
        };

        match Trainer::attach(pid) {
            Ok(trainer) => {
                self.info(format!("已连接 PID {pid}"));
                self.info(format!("mono 运行时：{}", trainer.mono_path()));
                self.trainer = Some(Arc::new(trainer));
                self.attached = true;
                self.pid = Some(pid);
                self.clear_located();
            }
            Err(e) => self.log(Level::Error, e),
        }
    }

    fn detach(&mut self) {
        self.trainer = None;
        self.attached = false;
        self.pid = None;
        self.busy = false;
        self.clear_located();
        self.info("已断开连接。");
    }

    // ---------- 后台任务 ----------

    fn start_set_lives(&mut self) {
        let Some(trainer) = self.trainer.clone() else {
            return;
        };
        let (Some(player), Some(lives)) = (self.parse_player(), self.parse_lives()) else {
            return;
        };

        self.busy = true;
        self.banner = Banner::None;
        self.info(format!("正在把玩家 {player} 的生命设为 {lives} …"));

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // 捕获 panic 并原样报出来。之前没有这一层，后台线程一 panic
            // 发送端就被 drop，界面只会永远停在「处理中」，什么线索都没有。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                trainer.set_lives(player, lives)
            }));
            let _ = tx.send(Outcome::Set(result.unwrap_or_else(|payload| Report {
                steps: Vec::new(),
                addr: None,
                error: Some(format!("内部错误（panic）：{}", panic_message(&payload))),
            })));
        });
    }

    /// 只读检测：定位生命值地址并读一下当前值。不写任何内存。
    fn start_detect(&mut self) {
        let Some(trainer) = self.trainer.clone() else {
            return;
        };
        let Some(player) = self.parse_player() else {
            return;
        };

        self.busy = true;
        self.info(format!("正在定位玩家 {player} 的生命值 …"));

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                trainer.find_lives_address(player)
            }));

            let outcome = match result {
                Ok(Ok((addr, steps))) => {
                    let value = trainer.peek_i32(addr);
                    DetectOutcome::Found { addr, value, steps }
                }
                Ok(Err((error, steps))) => DetectOutcome::NotFound { error, steps },
                Err(payload) => DetectOutcome::Panic(panic_message(&payload)),
            };
            let _ = tx.send(Outcome::Detect(outcome));
        });
    }

    fn pump(&mut self, ctx: &egui::Context) {
        loop {
            match self.rx.try_recv() {
                Ok(Outcome::Set(report)) => {
                    self.busy = false;
                    for s in report.steps {
                        self.info(s);
                    }
                    match report.error {
                        Some(e) => {
                            self.log(Level::Error, e.clone());
                            self.banner = Banner::Err(e);
                        }
                        None => {
                            self.lives_addr = report.addr;
                            self.force_poll = true;
                            self.log(Level::Ok, "写入成功。");
                            self.banner = Banner::Ok("已写入，看游戏里的生命数。".to_owned());
                        }
                    }
                }
                Ok(Outcome::Detect(outcome)) => {
                    self.busy = false;
                    match outcome {
                        DetectOutcome::Found { addr, value, steps } => {
                            for s in steps {
                                self.info(s);
                            }
                            self.lives_addr = Some(addr);
                            self.lives_now = value;
                            self.force_poll = true;
                            match value {
                                Some(v) => self
                                    .log(Level::Ok, format!("当前生命 = {v}（地址 0x{addr:X}）")),
                                None => self.log(
                                    Level::Warn,
                                    format!("地址 0x{addr:X} 已定位，但读不到值。"),
                                ),
                            }
                        }
                        DetectOutcome::NotFound { error, steps } => {
                            for s in steps {
                                self.info(s);
                            }
                            self.lives_addr = None;
                            self.lives_now = None;
                            self.log(Level::Error, error);
                        }
                        DetectOutcome::Panic(msg) => {
                            self.log(Level::Error, format!("内部错误（panic）：{msg}"));
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // 发送端没了 = 后台线程没了。正常情况下 catch_unwind
                    // 已经把它转成 Outcome 发出来了，走到这里说明连发送都没来得及。
                    if self.busy {
                        self.busy = false;
                        self.log(Level::Error, "后台线程异常退出，操作未完成。");
                    }
                    break;
                }
            }
            ctx.request_repaint();
        }
    }

    fn check_alive(&mut self) {
        let alive = self.trainer.as_ref().map(|t| t.is_alive()).unwrap_or(false);
        if self.attached && !alive {
            self.log(Level::Error, "游戏进程已退出。");
            self.detach();
        }
    }

    /// 刷新「当前生命」的显示。拿到的地址是稳定的，这里只做普通内存读。
    fn poll_lives(&mut self) {
        let Some(addr) = self.lives_addr else {
            return;
        };
        if !self.force_poll && self.last_poll.elapsed() < POLL_INTERVAL {
            return;
        }
        self.last_poll = Instant::now();
        self.force_poll = false;

        if let Some(t) = &self.trainer {
            self.lives_now = t.peek_i32(addr);
        }
    }

    // ---------- 绘制 ----------

    fn header(&self, ui: &mut egui::Ui) {
        ui.label(
            egui::RichText::new("BROFORCE 修改器")
                .size(19.0)
                .strong()
                .color(palette::TEXT),
        );
        ui.label(
            egui::RichText::new("一键把生命改成 999")
                .size(12.0)
                .color(palette::DIM),
        );
    }

    fn connect_card(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        card(ui, |ui| {
            // 状态胶囊就放在标题同一行。之前放在窗口标题栏右端做右对齐，
            // 结果在 `ui.horizontal` 里拿不到宽度、根本没渲染出来；
            // 放在这里既稳，离「连接」这个动作也更近。
            let connected = self.attached;
            let pid = self.pid;
            ui.horizontal(|ui| {
                section_title(ui, "1", "连接游戏");
                ui.add_space(8.0);
                status_pill(ui, connected, pid);
            });
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.add_enabled(
                    !self.attached,
                    egui::TextEdit::singleline(&mut self.exe_name)
                        .desired_width(170.0)
                        .hint_text("进程名"),
                );
                if self.attached {
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("断开"))
                        .clicked()
                    {
                        *action = Some(Action::Detach);
                    }
                } else if ui.button("连接").clicked() {
                    *action = Some(Action::Attach);
                }
            });

            ui.add_space(6.0);
            let hint = if self.attached {
                match self.pid {
                    Some(pid) => format!("已连接 · PID {pid} · mono 运行时已解析"),
                    None => "已连接".to_owned(),
                }
            } else {
                "先启动 Broforce，再点「连接」。".to_owned()
            };
            ui.label(
                egui::RichText::new(hint)
                    .size(11.0)
                    .color(if self.attached {
                        palette::OK
                    } else {
                        palette::DIM
                    }),
            );
        });
    }

    fn action_card(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        card(ui, |ui| {
            ui.horizontal(|ui| section_title(ui, "2", "修改生命"));
            ui.add_space(8.0);

            let editable = self.attached && !self.busy;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("玩家").size(12.0).color(palette::DIM));
                ui.add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut self.player).desired_width(34.0),
                );
                ui.add_space(12.0);
                ui.label(egui::RichText::new("生命").size(12.0).color(palette::DIM));
                ui.add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut self.lives).desired_width(64.0),
                );
            });
            ui.add_space(10.0);

            // 主操作：整宽、琥珀色，视线一眼落上去
            let label = if self.busy {
                "处理中…"
            } else {
                "设为 999 生命"
            };
            let primary = egui::Button::new(
                egui::RichText::new(label)
                    .size(14.0)
                    .strong()
                    .color(palette::BG),
            )
            .fill(palette::ACCENT)
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(ui.available_width(), 36.0));
            if ui.add_enabled(editable, primary).clicked() {
                *action = Some(Action::Apply);
            }

            // 只读检测 + 实时读数
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(editable, egui::Button::new("检测当前生命"))
                    .on_hover_text("只读：定位生命值在哪，并读出当前数值，不修改任何内存")
                    .clicked()
                {
                    *action = Some(Action::Detect);
                }

                match (self.lives_addr, self.lives_now) {
                    (Some(_), Some(v)) => {
                        ui.label(
                            egui::RichText::new(format!("当前 {v}"))
                                .size(13.0)
                                .monospace()
                                .color(palette::OK),
                        );
                    }
                    (Some(_), None) => {
                        ui.label(egui::RichText::new("已定位").size(12.0).color(palette::DIM));
                    }
                    _ => {}
                }
            });

            match &self.banner {
                Banner::None => {}
                Banner::Ok(msg) => banner(ui, palette::OK, "✓", msg),
                Banner::Err(msg) => banner(ui, palette::ERR, "✕", msg),
            }
        });
    }

    fn log_card(&mut self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("运行日志")
                        .size(13.0)
                        .strong()
                        .color(palette::TEXT),
                );
                ui.add_space(8.0);
                if ui
                    .small_button("复制")
                    .on_hover_text("把日志复制到剪贴板，便于反馈问题")
                    .clicked()
                {
                    *action = Some(Action::CopyLog);
                }
                if ui
                    .small_button("清空")
                    .on_hover_text("清掉当前日志")
                    .clicked()
                {
                    *action = Some(Action::ClearLog);
                }
            });
            ui.add_space(6.0);

            // 固定高度，外面还有一层滚动兜底，窗口再矮也不会把内容顶掉
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for line in &self.log {
                        ui.label(
                            egui::RichText::new(format!("{}  {}", line.mark(), line.text))
                                .size(11.5)
                                .monospace()
                                .color(line.color()),
                        );
                    }
                });
        });
    }
}

impl eframe::App for TrainerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        self.check_alive();
        self.poll_lives();
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(80));
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut action: Option<Action> = None;

        // 回车直接执行主操作，但不抢输入框里的回车
        let enter = self.attached
            && !self.busy
            && !root.ctx().text_edit_focused()
            && root.ctx().input(|i| i.key_pressed(egui::Key::Enter));

        // 所有内容放在同一个 CentralPanel 里，外面套一层滚动。
        // 试过用 Panel::bottom 把日志钉在底下，但那个面板实测只肯拿 67px
        // （default_size / exact_size 都没按预期生效），日志区几乎看不见。
        // 单面板 + 外层滚动虽然没有「日志固定」那么好看，但行为完全可预测。
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette::BG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(root, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.header(ui);
                        ui.add_space(12.0);
                        self.connect_card(ui, &mut action);
                        ui.add_space(10.0);
                        self.action_card(ui, &mut action);
                        ui.add_space(10.0);
                        self.log_card(ui, &mut action);
                    });
            });

        if enter {
            action = Some(Action::Apply);
        }

        match action {
            Some(Action::Attach) => self.attach(),
            Some(Action::Detach) => self.detach(),
            Some(Action::Apply) => self.start_set_lives(),
            Some(Action::Detect) => self.start_detect(),
            Some(Action::ClearLog) => {
                self.log.clear();
                self.info("日志已清空。");
            }
            Some(Action::CopyLog) => {
                let text: String = self
                    .log
                    .iter()
                    .map(|l| format!("{} {}", l.mark(), l.text))
                    .collect::<Vec<_>>()
                    .join("\n");
                root.ctx().copy_text(text);
                self.info("日志已复制到剪贴板。");
            }
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 绘制小工具
// ---------------------------------------------------------------------------

/// 一张卡片。
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(palette::CARD)
        .stroke(egui::Stroke::new(1.0, palette::BORDER))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// 「1 连接游戏」这样的小标题。
///
/// 只往当前布局里放两个 label，不自己开 `ui.horizontal`——否则嵌在
/// 调用方的 horizontal 里会抢宽度，后面的东西就摆不下了。
fn section_title(ui: &mut egui::Ui, step: &str, title: &str) {
    ui.label(
        egui::RichText::new(step)
            .size(13.0)
            .strong()
            .color(palette::ACCENT),
    );
    ui.label(
        egui::RichText::new(title)
            .size(13.0)
            .strong()
            .color(palette::TEXT),
    );
}

/// 右上角的连接状态胶囊。
fn status_pill(ui: &mut egui::Ui, connected: bool, pid: Option<u32>) {
    let color = if connected { palette::OK } else { palette::DIM };
    let text = match (connected, pid) {
        (true, Some(pid)) => format!("● 已连接 {pid}"),
        (true, None) => "● 已连接".to_owned(),
        _ => "● 未连接".to_owned(),
    };

    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(egui::CornerRadius::same(20))
        .inner_margin(egui::Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(11.5).strong().color(color));
        });
}

/// 操作结果横幅。
fn banner(ui: &mut egui::Ui, color: egui::Color32, mark: &str, msg: &str) {
    ui.add_space(8.0);
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(format!("{mark}  {msg}"))
                    .size(12.0)
                    .color(color),
            );
        });
}

/// 全局配色与圆角。
///
/// 注意这里必须**显式锁死深色主题**：egui 会跟随系统主题在 Dark / Light
/// 两套 `Visuals` 之间切换，只调 `set_visuals` 的话，系统是浅色时
/// 输入框、按钮这些控件会被换回浅色那套配色，跟深色卡片完全不搭。
fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = palette::BG;
    visuals.window_fill = palette::CARD;
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x0D, 0x0F, 0x13);
    visuals.faint_bg_color = palette::CARD;
    visuals.override_text_color = Some(palette::TEXT);
    visuals.hyperlink_color = palette::ACCENT;
    visuals.selection.bg_fill = palette::ACCENT.gamma_multiply(0.35);

    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
    ] {
        widget.corner_radius = egui::CornerRadius::same(6);
    }
    visuals.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(0x26, 0x2C, 0x36);
    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0x33, 0x3B, 0x47);
    visuals.widgets.active.weak_bg_fill = egui::Color32::from_rgb(0x3D, 0x46, 0x54);

    // 输入框的底色单独走 extreme_bg_color，给一个比卡片更深的值
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x0D, 0x0F, 0x13);

    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, visuals.clone());
    ctx.set_visuals(visuals);
}

/// 从 `catch_unwind` 拿到的 payload 里取出可读的 panic 信息。
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "（无法读取 panic 信息）".to_owned()
    }
}

/// egui 自带字体没有中文字形，这里从系统里挂一个中文字体上去。
fn install_cjk_font(ctx: &egui::Context) {
    const CANDIDATES: [&str; 4] = [
        "C:/Windows/Fonts/msyh.ttc",   // 微软雅黑
        "C:/Windows/Fonts/Deng.ttf",   // 等线
        "C:/Windows/Fonts/simhei.ttf", // 黑体
        "C:/Windows/Fonts/simsun.ttc", // 宋体
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );

        // 放在最前面：优先用它，缺字形时再回落到 egui 内置字体。
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());

        ctx.set_fonts(fonts);
        return;
    }
}
