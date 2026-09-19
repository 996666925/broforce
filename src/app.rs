//! Slint 界面和业务逻辑之间的胶水层。
//!
//! 界面（`ui/app.slint`）只负责画和收集点击，任何状态都不留在那边；
//! 状态全在这个文件里。每次改动之后调一次 [`push_state`]，把该变的属性
//! 一次性推过去，界面不需要自己推导任何东西。
//!
//! 后台线程不直接碰界面：做完事把结果丢进 channel，由 [`TrainerApp`] 的
//! 心跳定时器取出来处理。这样界面线程永远不会被远程内存操作卡住。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

// `ComponentHandle` 提供 `run()` / `as_weak()`，不带进来这两个方法会「找不到」。
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::trainer::{Report, Steps, Trainer};
use crate::win32;
use crate::{AppWindow, LogRow};

/// 默认要找的可执行文件名。Steam 现版本用的是 `Broforce_beta.exe`。
const DEFAULT_EXE: &str = "Broforce_beta.exe";
/// 备用名，免得用户还得自己去改。
const FALLBACK_EXE: &str = "Broforce.exe";
/// 已经拿到地址之后，刷新「当前生命」的间隔。只是读内存，很便宜。
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// 心跳：取后台结果、看游戏还在不在、刷新生命读数。
const TICK: Duration = Duration::from_millis(100);
/// 日志条数上限。界面里是固定行高，条数只影响滚动区长度。
const MAX_LOG: usize = 300;

// 日志级别，和 `ui/app.slint` 里 `LogRow.tone` 的取值一一对应。
const INFO: i32 = 0;
const OK: i32 = 1;
const WARN: i32 = 2;
const ERR: i32 = 3;

fn mark_of(tone: i32) -> &'static str {
    match tone {
        OK => "✓",
        WARN => "!",
        ERR => "✕",
        _ => "·",
    }
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

struct State {
    attached: bool,
    pid: Option<u32>,

    trainer: Option<Arc<Trainer>>,
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
    busy: bool,

    /// 已经定位到的生命值地址；拿到之后只做普通内存读，不再建远程线程。
    lives_addr: Option<usize>,
    lives_now: Option<i32>,
    last_poll: Instant,
    force_poll: bool,

    /// 按钮下方的结果横幅：`(级别, 文本)`，级别 0 表示不显示。
    banner: (i32, String),
    logs: Rc<VecModel<LogRow>>,
}

impl State {
    fn push(&mut self, tone: i32, text: impl Into<SharedString>) {
        while self.logs.row_count() >= MAX_LOG {
            self.logs.remove(0);
        }
        self.logs.push(LogRow {
            mark: mark_of(tone).into(),
            text: text.into(),
            tone,
        });
    }

    fn clear_banner(&mut self) {
        self.banner = (0, String::new());
    }

    fn clear_located(&mut self) {
        self.lives_addr = None;
        self.lives_now = None;
        self.clear_banner();
    }
}

pub struct TrainerApp {
    ui: AppWindow,
    st: Rc<RefCell<State>>,
    /// 心跳定时器。**必须一直持有**——`Timer` 一被 drop 就停了。
    tick: slint::Timer,
}

impl TrainerApp {
    pub fn new() -> Result<Self, slint::PlatformError> {
        let ui = AppWindow::new()?;

        let (tx, rx) = channel();
        let logs = Rc::new(VecModel::<LogRow>::from(Vec::new()));
        ui.set_logs(ModelRc::from(logs.clone()));
        ui.set_exe_name(DEFAULT_EXE.into());

        let st = Rc::new(RefCell::new(State {
            attached: false,
            pid: None,
            trainer: None,
            tx,
            rx,
            busy: false,
            lives_addr: None,
            lives_now: None,
            last_poll: Instant::now(),
            force_poll: false,
            banner: (0, String::new()),
            logs,
        }));

        {
            let mut s = st.borrow_mut();
            s.push(INFO, "用法：启动 Broforce 并进入关卡 → 连接 → 点「设为 999 生命」。");
            s.push(WARN, "仅限单人模式，联机使用会影响其他玩家。");
        }

        let app = Self {
            ui,
            st,
            tick: slint::Timer::default(),
        };
        app.wire();
        app.refresh();
        app.start_tick();
        Ok(app)
    }

    pub fn run(self) -> Result<(), slint::PlatformError> {
        self.ui.run()
    }

    /// 把每个按钮 / 事件接到对应的处理函数上。
    ///
    /// 回调里只拿得到 `'static` 闭包，所以统一走 [`dispatch`]：
    /// 升级界面弱引用 + 借出状态，再交给真正的处理函数。
    fn wire(&self) {
        let st = self.st.clone();
        let weak = self.ui.as_weak();

        macro_rules! bind {
            ($setter:ident, $handler:path) => {{
                let st = st.clone();
                let weak = weak.clone();
                self.ui.$setter(move || dispatch(&weak, &st, $handler));
            }};
        }

        bind!(on_attach, attach);
        bind!(on_detach, detach);
        bind!(on_apply, apply);
        bind!(on_detect, detect);
        bind!(on_clear_log, clear_log);
        bind!(on_copy_log, copy_log);
    }

    fn start_tick(&self) {
        let st = self.st.clone();
        let weak = self.ui.as_weak();

        self.tick.start(slint::TimerMode::Repeated, TICK, move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut s = st.borrow_mut();
            pump(&ui, &mut s);
            check_alive(&ui, &mut s);
            poll_lives(&ui, &mut s);
        });
    }

    fn refresh(&self) {
        push_state(&self.ui, &self.st.borrow());
    }
}

/// 回调的公共入口：拿到界面句柄和状态之后转交给处理函数。
fn dispatch(
    weak: &slint::Weak<AppWindow>,
    st: &Rc<RefCell<State>>,
    handler: fn(&AppWindow, &mut State),
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let mut state = st.borrow_mut();
    handler(&ui, &mut state);
}

/// 把状态同步到界面属性上。界面不做任何推导，这里写什么就是什么。
fn push_state(ui: &AppWindow, s: &State) {
    ui.set_attached(s.attached);
    ui.set_busy(s.busy);

    ui.set_status_text(
        match (s.attached, s.pid) {
            (true, Some(pid)) => format!("已连接 {pid}"),
            (true, None) => "已连接".to_owned(),
            _ => "未连接".to_owned(),
        }
        .into(),
    );

    ui.set_hint_text(
        if s.attached {
            match s.pid {
                Some(pid) => format!("已连接 · PID {pid} · mono 运行时已解析"),
                None => "已连接".to_owned(),
            }
        } else {
            "先启动 Broforce，再点「连接」。".to_owned()
        }
        .into(),
    );

    // 只有同时有地址和读数值才算「已知」；只看得到地址时显示占位符，
    // 免得把「读不到」伪装成「生命是 0」。
    let (now, known) = match (s.lives_addr, s.lives_now) {
        (Some(_), Some(v)) => (v.to_string(), true),
        _ => ("—".to_owned(), false),
    };
    ui.set_lives_now_text(now.into());
    ui.set_lives_known(known);
    ui.set_lives_addr_text(
        match s.lives_addr {
            Some(addr) => format!("已定位 0x{addr:X}"),
            None => "尚未定位内存地址".to_owned(),
        }
        .into(),
    );

    ui.set_banner_tone(s.banner.0);
    ui.set_banner_text(s.banner.1.clone().into());
}

// ---------------------------------------------------------------------------
// 按钮处理
// ---------------------------------------------------------------------------

fn attach(ui: &AppWindow, s: &mut State) {
    let wanted = ui.get_exe_name().trim().to_owned();
    let mut pids = win32::find_pids(&wanted);

    if pids.is_empty() && !wanted.eq_ignore_ascii_case(FALLBACK_EXE) {
        pids = win32::find_pids(FALLBACK_EXE);
        if !pids.is_empty() {
            s.push(INFO, format!("没找到 {wanted}，改用 {FALLBACK_EXE}"));
            // 输入框是两向绑定的，写属性就等于同时把框里的字也换掉。
            ui.set_exe_name(FALLBACK_EXE.into());
        }
    }

    let Some(&pid) = pids.first() else {
        s.push(ERR, format!("没有找到进程 {wanted}，请先启动游戏。"));
        push_state(ui, s);
        return;
    };

    match Trainer::attach(pid) {
        Ok(trainer) => {
            s.push(INFO, format!("已连接 PID {pid}"));
            s.push(INFO, format!("mono 运行时：{}", trainer.mono_path()));
            s.trainer = Some(Arc::new(trainer));
            s.attached = true;
            s.pid = Some(pid);
            s.clear_located();
        }
        Err(e) => s.push(ERR, e),
    }
    push_state(ui, s);
}

fn detach(ui: &AppWindow, s: &mut State) {
    s.trainer = None;
    s.attached = false;
    s.pid = None;
    s.busy = false;
    s.clear_located();
    s.push(INFO, "已断开连接。");
    push_state(ui, s);
}

fn apply(ui: &AppWindow, s: &mut State) {
    let Some(trainer) = s.trainer.clone() else {
        return;
    };
    let (Some(player), Some(lives)) = (parse_player(ui, s), parse_lives(ui, s)) else {
        push_state(ui, s);
        return;
    };

    s.busy = true;
    s.clear_banner();
    s.push(INFO, format!("正在把玩家 {player} 的生命设为 {lives} …"));
    push_state(ui, s);

    let tx = s.tx.clone();
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
fn detect(ui: &AppWindow, s: &mut State) {
    let Some(trainer) = s.trainer.clone() else {
        return;
    };
    let Some(player) = parse_player(ui, s) else {
        push_state(ui, s);
        return;
    };

    s.busy = true;
    s.clear_banner();
    s.push(INFO, format!("正在定位玩家 {player} 的生命值 …"));
    push_state(ui, s);

    let tx = s.tx.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            trainer.find_lives_address(player)
        }));

        let outcome = match result {
            Ok(Ok((addr, steps))) => DetectOutcome::Found {
                addr,
                value: trainer.peek_i32(addr),
                steps,
            },
            Ok(Err((error, steps))) => DetectOutcome::NotFound { error, steps },
            Err(payload) => DetectOutcome::Panic(panic_message(&payload)),
        };
        let _ = tx.send(Outcome::Detect(outcome));
    });
}

fn clear_log(ui: &AppWindow, s: &mut State) {
    s.logs.clear();
    s.push(INFO, "日志已清空。");
    push_state(ui, s);
}

fn copy_log(ui: &AppWindow, s: &mut State) {
    let text = (0..s.logs.row_count())
        .filter_map(|i| s.logs.row_data(i))
        .map(|row| format!("{} {}", row.mark, row.text))
        .collect::<Vec<_>>()
        .join("\n");

    if win32::set_clipboard_text(&text) {
        s.push(INFO, "日志已复制到剪贴板。");
    } else {
        s.push(WARN, "剪贴板被别的程序占着，复制失败，稍后再试。");
    }
    push_state(ui, s);
}

// ---------------------------------------------------------------------------
// 心跳里跑的三件事
// ---------------------------------------------------------------------------

fn parse_player(ui: &AppWindow, s: &mut State) -> Option<i32> {
    match ui.get_player().trim().parse::<i32>() {
        Ok(v) if (0..4).contains(&v) => Some(v),
        _ => {
            s.push(ERR, "玩家序号要填 0 到 3 之间的整数。");
            None
        }
    }
}

fn parse_lives(ui: &AppWindow, s: &mut State) -> Option<i32> {
    match ui.get_lives().trim().parse::<i32>() {
        Ok(v) => Some(v),
        Err(_) => {
            s.push(ERR, "生命值要填整数。");
            None
        }
    }
}

fn pump(ui: &AppWindow, s: &mut State) {
    let mut dirty = false;

    loop {
        match s.rx.try_recv() {
            Ok(Outcome::Set(report)) => {
                s.busy = false;
                dirty = true;
                for step in report.steps {
                    s.push(INFO, step);
                }
                match report.error {
                    Some(e) => {
                        s.push(ERR, e.clone());
                        s.banner = (2, e);
                    }
                    None => {
                        s.lives_addr = report.addr;
                        s.force_poll = true;
                        s.push(OK, "写入成功。");
                        s.banner = (1, "已写入，看游戏里的生命数。".to_owned());
                    }
                }
            }
            Ok(Outcome::Detect(outcome)) => {
                s.busy = false;
                dirty = true;
                match outcome {
                    DetectOutcome::Found { addr, value, steps } => {
                        for step in steps {
                            s.push(INFO, step);
                        }
                        s.lives_addr = Some(addr);
                        s.lives_now = value;
                        s.force_poll = true;
                        match value {
                            Some(v) => {
                                s.push(OK, format!("当前生命 = {v}（地址 0x{addr:X}）"));
                            }
                            None => {
                                s.push(WARN, format!("地址 0x{addr:X} 已定位，但读不到值。"));
                            }
                        }
                    }
                    DetectOutcome::NotFound { error, steps } => {
                        for step in steps {
                            s.push(INFO, step);
                        }
                        s.lives_addr = None;
                        s.lives_now = None;
                        s.push(ERR, error);
                    }
                    DetectOutcome::Panic(msg) => {
                        s.push(ERR, format!("内部错误（panic）：{msg}"));
                    }
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                // 发送端没了 = 后台线程没了。正常情况下 catch_unwind
                // 已经把它转成 Outcome 发出来了，走到这里说明连发送都没来得及。
                if s.busy {
                    s.busy = false;
                    dirty = true;
                    s.push(ERR, "后台线程异常退出，操作未完成。");
                }
                break;
            }
        }
    }

    if dirty {
        push_state(ui, s);
    }
}

fn check_alive(ui: &AppWindow, s: &mut State) {
    let alive = s.trainer.as_ref().map(|t| t.is_alive()).unwrap_or(false);
    if s.attached && !alive {
        s.push(ERR, "游戏进程已退出。");
        detach(ui, s);
    }
}

/// 刷新「当前生命」的显示。拿到的地址是稳定的，这里只做普通内存读。
fn poll_lives(ui: &AppWindow, s: &mut State) {
    let Some(addr) = s.lives_addr else {
        return;
    };
    if !s.force_poll && s.last_poll.elapsed() < POLL_INTERVAL {
        return;
    }
    s.last_poll = Instant::now();
    s.force_poll = false;

    let now = s.trainer.as_ref().and_then(|t| t.peek_i32(addr));
    if now != s.lives_now {
        s.lives_now = now;
        push_state(ui, s);
    }
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
