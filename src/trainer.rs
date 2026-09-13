//! 把「把某号玩家的生命设成 N」这件事串起来。
//!
//! ## 为什么是直接写内存，而不是调游戏的 `SetLives`
//!
//! 最初的做法是在游戏进程里远程调用 `HeroController.SetLives(0, 999)`，
//! 语义上最干净。但它把游戏搞崩了两次，两次都是同一个确定性错误：
//! `UnityPlayer.dll +0x81029C` 访问冲突。原因是 `SetLives` 会去刷 HUD，
//! 而 Unity 的 API 只能在主线程调用——我们的远程线程不是 Unity 线程。
//!
//! 所以现在改成：只用 mono 的**只读**查询拿到两个地址——
//!
//!   1. `HeroController.players` 这个静态字段自身的地址（里面存着 `Player[]` 引用）；
//!   2. `Player.lives` 这个实例字段在对象内的偏移。
//!
//! 然后由我们自己的进程读出数组、找到玩家对象、直接写那 4 个字节。
//!
//! 全程不执行任何游戏逻辑。HUD 会在主线程的下一帧自己读到新值。

use std::sync::Arc;

use crate::inject;
use crate::mono::{MonoApi, MonoFunctions};
use crate::win32::Process;

/// 承载一次操作的过程记录，用来在界面上逐步显示。
pub type Steps = Vec<String>;

/// 数组对象头的读取长度。够覆盖到第 4 个元素（起点 +0x20，每元素 8 字节）。
const ARRAY_HEADER_LEN: usize = 0x40;

/// 在数组对象头里按候选偏移取出第 `index` 个元素。
///
/// `is_player` 用来确认某个指针确实指向 Player 对象。**这是唯一的硬判据**——
/// 即使对数组头布局的判断过时了，也不会认错对象。
///
/// 返回值里的第二个分量是命中的元素起点，方便排查时看清实际布局。
fn pick_element(
    header: &[u8],
    index: usize,
    mut is_player: impl FnMut(u64) -> bool,
) -> Option<(u64, usize)> {
    for elem_off in ARRAY_ELEMENT_OFFSETS {
        let at = elem_off + index * 8;
        if at + 8 > header.len() {
            continue;
        }
        let ptr = u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
        if ptr != 0 && is_player(ptr) {
            return Some((ptr, elem_off));
        }
    }
    None
}

/// 从若干候选偏移里挑出第一个像样的数组长度。
fn pick_length(candidates: impl IntoIterator<Item = Option<u64>>) -> Option<u64> {
    candidates
        .into_iter()
        .flatten()
        .find(|&v| (1..=64).contains(&v))
}

/// 一次操作的结果。
///
/// `steps` 无论成功失败都会带回来——失败的现场往往比成功的更有用，
/// 早先的写法在出错时把 steps 丢了，界面上只剩一句没头没尾的报错。
pub struct Report {
    pub steps: Steps,
    pub error: Option<String>,
    /// 成功时是写入的地址，界面拿它接着刷新「当前生命」。
    pub addr: Option<usize>,
}

/// Mono 数组的元素起点候选。
///
/// 实测（对着真实进程读出来的）：这个 Unity 自带的 Mono 里，
/// 数组是 16 字节对象头，**长度在 +0x18，元素从 +0x20 开始**。
/// 起初按「长度紧跟对象头」的常见布局读了 +0x10，读到 0 就提前退出了。
///
/// 不同版本的 Mono 数组头不一样，所以这里两个都试，
/// 最终由「对象前 8 字节 == Player 的 vtable」定夺——那条才是硬判据。
const ARRAY_ELEMENT_OFFSETS: [usize; 2] = [0x20, 0x18];

/// 长度可能所在的位置。只用来在报错时说清楚「一共有几个玩家」，
/// 不参与定位——定位完全靠 vtable 校验。
const ARRAY_LENGTH_OFFSETS: [usize; 2] = [0x18, 0x10];

pub struct Trainer {
    proc: Arc<Process>,
    fns: MonoFunctions,
    mono_path: String,
}

impl Trainer {
    /// 附加到进程并解析 mono 函数地址。这一步不修改游戏任何状态。
    pub fn attach(pid: u32) -> Result<Self, String> {
        let proc = Arc::new(Process::open(pid)?);

        let api = MonoApi::locate(&proc)?;
        let fns = MonoFunctions::resolve(&api)?;
        let mono_path = api.dll_path.clone();

        Ok(Self {
            proc,
            fns,
            mono_path,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.proc.is_alive()
    }

    /// 目标进程里 mono.dll 的位置，仅用于在界面上显示诊断信息。
    pub fn mono_path(&self) -> &str {
        &self.mono_path
    }

    /// 定位第 `player` 号玩家的 `lives` 字段地址。只读，不写任何内存。
    ///
    /// 目前只有只读诊断测试在用它，但保留为公开接口：
    /// 排查「到底定位到哪儿了」的时候，它比直接改要安全得多。
    #[allow(dead_code)]
    pub fn find_lives_address(&self, player: i32) -> Result<(usize, Steps), (String, Steps)> {
        let mut steps = Steps::new();
        match self.resolve_lives_address(player, &mut steps) {
            Ok(addr) => Ok((addr, steps)),
            Err(e) => Err((e, steps)),
        }
    }

    fn resolve_lives_address(&self, player: i32, steps: &mut Steps) -> Result<usize, String> {
        // ---- 1. HeroController.players：静态字段，里面存着 Player[] 引用 ----
        let players = inject::resolve_field(&self.proc, &self.fns, "HeroController", "players")?;
        if !players.is_complete() {
            return Err(format!(
                "没能定位 HeroController.players：klass=0x{:X} vtable=0x{:X} static=0x{:X}",
                players.klass, players.vtable, players.static_data
            ));
        }
        steps.push(format!(
            "HeroController：klass=0x{:X} vtable=0x{:X} static=0x{:X}",
            players.klass, players.vtable, players.static_data
        ));
        steps.push(format!(
            "HeroController.players 字段偏移 {} → 字段地址 0x{:X}",
            players.offset,
            players.static_field_addr()
        ));

        // ---- 2. Player.lives：实例字段偏移；顺带拿到 Player 的 vtable 用于校验 ----
        let lives = inject::resolve_field(&self.proc, &self.fns, "Player", "lives")?;
        if !lives.is_complete() {
            return Err(format!(
                "没能定位 Player.lives：klass=0x{:X} vtable=0x{:X} static=0x{:X}",
                lives.klass, lives.vtable, lives.static_data
            ));
        }
        steps.push(format!(
            "Player：klass=0x{:X} vtable=0x{:X}，lives 偏移 {}",
            lives.klass, lives.vtable, lives.offset
        ));

        // ---- 3. 读出 Player[] 引用 ----
        let field_addr = players.static_field_addr() as usize;
        let array = match self.proc.read_u64(field_addr) {
            None => {
                return Err(format!(
                    "读不到 HeroController.players 字段的内容（地址 0x{field_addr:X} 不可读）"
                ));
            }
            Some(0) => {
                return Err("players 是 null —— 多半是还没进入关卡，玩家数组尚未建立。".into());
            }
            Some(v) => v,
        };
        steps.push(format!("players 数组对象 @ 0x{array:X}"));

        // ---- 4. 找到玩家对象 ----
        let object = self
            .locate_player(array, player, lives.vtable, lives.offset)
            .map_err(|e| {
                format!("{e}\n（诊断：字段地址 0x{field_addr:X}，数组对象 0x{array:X}）")
            })?;
        steps.push(format!(
            "玩家 {player} 的对象 @ 0x{object:X}（已按 vtable 校验确实是 Player）"
        ));

        Ok(object as usize + lives.offset as usize)
    }

    /// 把一段内存按 8 字节一组 dump 成可读字符串，用于诊断布局。
    fn dump_qwords(&self, addr: usize, count: usize) -> String {
        let mut buf = vec![0u8; count * 8];
        if !self.proc.read(addr, &mut buf) {
            return format!("0x{addr:X} 起 {count} 个 qword 读不到");
        }
        (0..count)
            .map(|i| {
                let v = u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
                format!("+{:02X}={:016X}", i * 8, v)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 在 `Player[]` 里取出第 `index` 个元素，并用 vtable 确认它真的是 `Player`。
    ///
    /// 全是 `ReadProcessMemory`，读不到就是读不到，不会像解引用野指针那样崩掉。
    fn locate_player(
        &self,
        array: u64,
        index: i32,
        player_vtable: u64,
        lives_offset: i32,
    ) -> Result<u64, String> {
        let base = array as usize;
        let index = index as usize;

        let mut header = [0u8; ARRAY_HEADER_LEN];
        if !self.proc.read(base, &mut header) {
            return Err(format!(
                "players 指向的 0x{base:X} 读不到，多半不是有效对象（字段偏移算错了？）"
            ));
        }

        // 长度只用于把「没有这个玩家」讲清楚，不参与定位
        let length = pick_length(
            ARRAY_LENGTH_OFFSETS
                .iter()
                .map(|&off| self.proc.read_u64(base + off)),
        );
        if let Some(len) = length
            && index >= len as usize
        {
            return Err(format!("players 里只有 {len} 个玩家，没有第 {index} 号"));
        }

        // 找出第 index 个元素。判据是「对象前 8 字节 == Player 的 vtable」，
        // 再顺带确认生命值是个像样的数。
        let mut rejected = Vec::new();
        let hit = pick_element(&header, index, |ptr| {
            let vt = self.proc.read_u64(ptr as usize);
            if vt != Some(player_vtable) {
                rejected.push(format!("0x{ptr:X} 的 vtable 是 {vt:?}，不是 Player"));
                return false;
            }
            match self.proc.read_i32(ptr as usize + lives_offset as usize) {
                Some(v) if (0..10_000).contains(&v) => true,
                Some(v) => {
                    rejected.push(format!("0x{ptr:X} 是 Player，但生命值 {v} 不像话"));
                    false
                }
                None => {
                    rejected.push(format!("0x{ptr:X} 是 Player，但读不到生命值"));
                    false
                }
            }
        });

        match hit {
            Some((object, elem_off)) => {
                if elem_off != ARRAY_ELEMENT_OFFSETS[0] {
                    // 布局和预期不同时留个痕迹，方便日后回归时发现
                    eprintln!(
                        "提示：players 元素起点是 +{elem_off:X}，不是预期的 +{:X}",
                        ARRAY_ELEMENT_OFFSETS[0]
                    );
                }
                Ok(object)
            }
            None => {
                let dump = self.dump_qwords(base, 8);
                Err(format!(
                    "没能从 players 数组里认出玩家 {index}（期望的 Player vtable = 0x{player_vtable:X}）。\n\
                     对象头：{dump}\n{}",
                    if rejected.is_empty() {
                        "两个候选元素起点都没取到非空指针。".to_owned()
                    } else {
                        rejected.join("\n")
                    }
                ))
            }
        }
    }

    /// 读一个整数。纯内存读，不碰游戏逻辑——界面用它显示「当前生命」。
    pub fn peek_i32(&self, addr: usize) -> Option<i32> {
        self.proc.read_i32(addr)
    }

    /// 把第 `player` 号玩家的生命设为 `lives`。
    pub fn set_lives(&self, player: i32, lives: i32) -> Report {
        let mut steps = Steps::new();
        let result = self.write_lives(player, lives, &mut steps);
        Report {
            addr: result.as_ref().ok().copied(),
            error: result.err(),
            steps,
        }
    }

    fn write_lives(&self, player: i32, lives: i32, steps: &mut Steps) -> Result<usize, String> {
        let addr = self.resolve_lives_address(player, steps)?;

        let before = self.proc.read_i32(addr);
        if !self.proc.write_i32(addr, lives) {
            return Err(format!(
                "写入 0x{addr:X} 失败（{}）",
                crate::win32::last_error()
            ));
        }
        let after = self.proc.read_i32(addr);

        steps.push(format!("0x{addr:X}：{before:?} → {after:?}"));
        if after != Some(lives) {
            return Err(format!("写完读回来是 {after:?}，不是 {lives}"));
        }
        Ok(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用**实测到的**数组头布局做回归。
    ///
    /// 下面这些数字是从真实游戏进程里读出来的：
    ///   +00 = 数组类的 vtable, +08 = 0, +10 = 0, +18 = 长度(4), +20 = 元素[0]
    /// 最初按「长度紧跟对象头在 +0x10」的常见布局实现，读到 0 就提前退出，
    /// 元素根本没被探测到。这条测试锁住真正可用的那个偏移。
    #[test]
    fn picks_element_from_measured_layout() {
        let mut header = [0u8; ARRAY_HEADER_LEN];
        header[0x00..0x08].copy_from_slice(&0x0000_018A_F8A0_5A68u64.to_le_bytes()); // 数组 vtable
        header[0x18..0x20].copy_from_slice(&4u64.to_le_bytes()); // 长度
        let player = 0x0000_018B_1DD8_44E0u64;
        header[0x20..0x28].copy_from_slice(&player.to_le_bytes()); // 元素[0]

        assert_eq!(
            pick_element(&header, 0, |p| p == player),
            Some((player, 0x20)),
            "应当认出 +0x20 处的元素"
        );
    }

    /// 认不出来的指针不能被当成玩家。
    #[test]
    fn rejects_non_player_pointers() {
        let mut header = [0u8; ARRAY_HEADER_LEN];
        header[0x20..0x28].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        header[0x18..0x20].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        assert_eq!(pick_element(&header, 0, |_| false), None);
    }

    /// 全 0 的数组、以及越界下标，都应当安全地返回 `None`。
    #[test]
    fn handles_null_and_out_of_range_elements() {
        let header = [0u8; ARRAY_HEADER_LEN];
        assert_eq!(pick_element(&header, 0, |_| true), None, "全 0 认不出东西");
        assert_eq!(
            pick_element(&header, 99, |_| true),
            None,
            "下标越界不能读越界内存"
        );
    }

    /// 长度探测要跳过不可能的值（0、超大数、读不到）。
    #[test]
    fn length_probing_skips_implausible_values() {
        // 实测：+0x10 是 0（不是长度），+0x18 才是 4
        assert_eq!(pick_length([Some(0), Some(4)]), Some(4));
        assert_eq!(pick_length([None, Some(2)]), Some(2));
        assert_eq!(pick_length([Some(0), Some(0)]), None);
        assert_eq!(pick_length([Some(9999), None]), None);
        assert_eq!(pick_length([None, None]), None);
    }

    /// 附加到真正跑起来的游戏，只用**只读**查询定位玩家 0 的生命值地址。
    /// 不写任何内存，因此不会改动游戏状态。
    ///
    /// `cargo test -- --ignored --nocapture locates_lives_in_live_game`
    #[test]
    #[ignore = "需要 Broforce 正在运行且在关卡内"]
    fn locates_lives_in_live_game() {
        let pids = crate::win32::find_pids("Broforce_beta.exe");
        let Some(&pid) = pids.first() else {
            println!("没有找到 Broforce_beta.exe，跳过。");
            return;
        };

        let trainer = Trainer::attach(pid).expect("附加并解析 mono");
        println!("mono 运行时：{}", trainer.mono_path());

        match trainer.find_lives_address(0) {
            Ok((addr, steps)) => {
                for s in &steps {
                    println!("  {s}");
                }
                let value = trainer.proc.read_i32(addr);
                println!("玩家 0 生命值地址 = 0x{addr:X}，当前值 = {value:?}");
                assert!(value.is_some(), "地址应当可读");
            }
            Err((e, steps)) => {
                // 没进关卡时 players 为 null，这是预期内的结果，不算失败。
                // 但中间步骤要打出来，定位错的时候全靠它。
                for s in &steps {
                    println!("  {s}");
                }
                println!("定位失败：{e}");
            }
        }
    }
}
