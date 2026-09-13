//! 在目标进程里生成并执行一小段 x64 机器码，用来做 mono 的**只读**元数据查询。
//!
//! 整段 stub 是手写机器码，位置无关——所有需要用到的绝对地址
//! （mono 函数地址、字符串地址）都以 `mov r64, imm64` 直接编进指令流。
//! 因为内存块是先 `VirtualAllocEx` 拿到基址再写内容，
//! 这些地址在生成代码时就已经是最终值。
//!
//! ## 这里为什么只做只读查询
//!
//! 早先的版本是远程调用游戏自己的 `HeroController.SetLives`，结果把游戏
//! 搞崩了两次（`UnityPlayer.dll +0x81029C`，访问冲突）。原因是那个方法会
//! 去刷 HUD，而 Unity 的 API 只能在主线程调用——我们的远程线程不是 Unity 线程。
//!
//! 所以现在改成：这里只**查元数据**（查类、查字段、拿偏移、拿 vtable），
//! 拿到地址之后由我们自己的进程直接读写内存。全程不执行任何游戏逻辑，
//! 也就没有线程亲和性的问题。

use crate::mono::MonoFunctions;
use crate::win32::Process;

const BLOCK_SIZE: usize = 0x1000;

// 内存块内的固定布局。代码放最前面，其余是常量与结果槽位。
const OFF_CODE: usize = 0x000;
const OFF_STR_IMAGE: usize = 0x200;
const OFF_STR_NS: usize = 0x220;
const OFF_STR_CLASS: usize = 0x240;
const OFF_STR_FIELD: usize = 0x260;
const OFF_SLOT_KLASS: usize = 0x300;
const OFF_SLOT_OFFSET: usize = 0x308;
const OFF_SLOT_VTABLE: usize = 0x310;
const OFF_SLOT_STATIC: usize = 0x318;

// ---------------------------------------------------------------------------
// 汇编发射器
// ---------------------------------------------------------------------------

/// 极简的 x64 指令发射器，只覆盖这段 stub 用得到的指令。
struct Asm {
    code: Vec<u8>,
}

impl Asm {
    fn new() -> Self {
        Self { code: Vec::new() }
    }

    fn b(&mut self, x: u8) {
        self.code.push(x);
    }

    fn bs(&mut self, x: &[u8]) {
        self.code.extend_from_slice(x);
    }

    /// `mov r64, imm64`。reg: 0=rax 1=rcx 2=rdx 3=rbx 7=rdi 8=r8。
    fn mov_imm(&mut self, reg: u8, value: u64) {
        let rex = if reg >= 8 { 0x49 } else { 0x48 };
        self.b(rex);
        self.b(0xB8 + (reg & 7));
        self.bs(&value.to_le_bytes());
    }

    /// `mov dst, src`（64 位，寄存器到寄存器）。
    fn mov_rr(&mut self, dst: u8, src: u8) {
        let rex = 0x48 | if src >= 8 { 0x04 } else { 0 } | if dst >= 8 { 0x01 } else { 0 };
        self.b(rex);
        self.b(0x89);
        // ModRM: mod=11, reg=源, rm=目的
        self.b(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    /// `mov [dst], src`，dst 为寄存器里存的地址。
    fn mov_store(&mut self, dst: u8, src: u8) {
        let rex = 0x48 | if src >= 8 { 0x04 } else { 0 } | if dst >= 8 { 0x01 } else { 0 };
        self.b(rex);
        self.b(0x89);
        // ModRM: mod=00（寄存器间接寻址）, reg=源, rm=目的
        self.b(((src & 7) << 3) | (dst & 7));
    }

    /// 把 src 寄存器的值写到绝对地址 `addr`（借用 rcx 做中转）。
    fn store_to(&mut self, addr: usize, src: u8) {
        self.mov_imm(1, addr as u64);
        self.mov_store(1, src);
    }

    /// `xor dst, src`（64 位）。
    fn xor_rr(&mut self, dst: u8, src: u8) {
        let rex = 0x48 | if src >= 8 { 0x04 } else { 0 } | if dst >= 8 { 0x01 } else { 0 };
        self.b(rex);
        self.b(0x31);
        self.b(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    /// `call rax`
    fn call_rax(&mut self) {
        self.bs(&[0xFF, 0xD0]);
    }

    /// `test rax, rax`
    fn test_rax(&mut self) {
        self.bs(&[0x48, 0x85, 0xC0]);
    }

    /// `jz rel32`（`0F 84`），先占位，稍后由 `patch_jz` 回填。
    /// 返回 disp32 的起始下标。
    ///
    /// 这里刻意用 rel32 而不是 rel8：解析 stub 里第一个守卫要跳过后面
    /// 四个查询块，距离有 190 多字节，rel8 的 ±127 根本不够。
    /// 早先用 rel8，`assert!` 直接在后台线程里 panic，界面就卡在「处理中」了。
    fn jz_placeholder(&mut self) -> usize {
        self.bs(&[0x0F, 0x84, 0x00, 0x00, 0x00, 0x00]);
        self.code.len() - 4
    }

    /// 把之前占位的 `jz` 指向指定位置。
    fn patch_jz_to(&mut self, at: usize, target: usize) {
        // disp32 是相对「该指令之后」的下一条指令算的
        let next = at + 4;
        let rel = target as i64 - next as i64;
        assert!(
            (i32::MIN as i64..=i32::MAX as i64).contains(&rel),
            "跳转距离超出 rel32 范围"
        );
        self.code[at..at + 4].copy_from_slice(&(rel as i32).to_le_bytes());
    }

    /// 函数序言：`push rbx; push rdi; sub rsp, 0x28`。
    ///
    /// 进入 stub 时 rsp ≡ 8 (mod 16)（`call` 压入了返回地址）。
    /// 两次 push 后是 8，再减 0x28 就回到 0 —— 满足 Win64 要求的
    /// 「`call` 前 rsp 必须 16 字节对齐」，0x28 也足够放下 32 字节影子空间。
    fn prologue(&mut self) {
        self.b(0x53); // push rbx
        self.b(0x57); // push rdi
        self.bs(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    }

    /// 函数尾声，最终返回 0（ThreadProc 的约定返回值）。
    fn epilogue(&mut self) {
        self.bs(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
        self.b(0x5F); // pop rdi
        self.b(0x5B); // pop rbx
        self.xor_rr(0, 0); // xor rax, rax
        self.b(0xC3); // ret
    }

    /// `mono_thread_attach(mono_get_root_domain())`，线程指针存进 rdi。
    fn thread_attach(&mut self, fns: &MonoFunctions) {
        self.mov_imm(0, fns.get_root_domain as u64);
        self.call_rax();
        self.mov_rr(1, 0); // rcx = domain
        self.mov_imm(0, fns.thread_attach as u64);
        self.call_rax();
        self.mov_rr(7, 0); // rdi = MonoThread*
    }

    /// `mono_thread_detach(rdi)`，避免反复调用时泄漏 mono 的线程对象。
    fn thread_detach(&mut self, fns: &MonoFunctions) {
        self.mov_rr(1, 7);
        self.mov_imm(0, fns.thread_detach as u64);
        self.call_rax();
    }
}

// ---------------------------------------------------------------------------
// 远程内存块
// ---------------------------------------------------------------------------

/// 目标进程里一块可读可写可执行的内存，析构时自动释放。
struct RemoteBlock<'a> {
    proc: &'a Process,
    base: usize,
}

impl<'a> RemoteBlock<'a> {
    fn new(proc: &'a Process) -> Result<Self, String> {
        let base = proc.alloc(BLOCK_SIZE).ok_or_else(|| {
            format!(
                "VirtualAllocEx 失败（{}）——目标进程可能已退出，或本程序需要以管理员身份运行",
                crate::win32::last_error()
            )
        })?;
        Ok(Self { proc, base })
    }

    fn at(&self, off: usize) -> usize {
        self.base + off
    }

    fn put(&self, off: usize, bytes: &[u8]) -> Result<(), String> {
        if self.proc.write(self.at(off), bytes) {
            Ok(())
        } else {
            Err(format!("写入远程内存失败 @ +0x{off:X}"))
        }
    }

    /// 写入一个以 NUL 结尾的 UTF-8 字符串。
    fn put_cstr(&self, off: usize, s: &str) -> Result<(), String> {
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        self.put(off, &bytes)
    }

    fn get_u64(&self, off: usize) -> Option<u64> {
        self.proc.read_u64(self.at(off))
    }

    fn clear(&self, off: usize) -> Result<(), String> {
        self.put(off, &[0u8; 8])
    }
}

impl Drop for RemoteBlock<'_> {
    fn drop(&mut self) {
        self.proc.free(self.base);
    }
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 一个字段在目标进程里的位置信息。
#[derive(Clone, Copy, Debug)]
pub struct FieldLayout {
    /// 声明该字段的 MonoClass。
    pub klass: u64,
    /// 该类的 MonoVTable。每个实例对象的前 8 字节就是它，可用来校验对象身份。
    pub vtable: u64,
    /// 字段在对象内的字节偏移。
    pub offset: i32,
    /// 类的静态字段数据区基址（静态字段地址 = 它 + `offset`）。
    pub static_data: u64,
}

impl FieldLayout {
    pub fn is_complete(&self) -> bool {
        self.klass != 0 && self.vtable != 0 && self.static_data != 0
    }

    /// 静态字段自身的绝对地址。
    pub fn static_field_addr(&self) -> u64 {
        self.static_data + self.offset as u32 as u64
    }
}

/// 生成好的 stub：机器码，加上各处 `jz` 的回填位置。
///
/// `jumps` 和 `guard_target` 只在测试里读——留着它们是为了让
/// 「守卫是否跳对了位置」这件事可以被断言，而不是只能靠肉眼看机器码。
struct Stub {
    code: Vec<u8>,
    /// 每个 `jz` 的 disp32 起始下标。
    #[allow(dead_code)]
    jumps: Vec<usize>,
    /// 所有守卫共同跳向的位置。
    #[allow(dead_code)]
    guard_target: usize,
}

/// 生成解析 stub 的机器码。
///
/// 抽成不碰进程的纯函数，就是为了能单测——早先这段逻辑内联在
/// `resolve_field` 里，跳转越界那个 bug 没有任何测试能拦住。
fn build_resolve_code(fns: &MonoFunctions, base: usize) -> Stub {
    let at = |off: usize| base + off;
    let mut a = Asm::new();
    let mut jumps = Vec::new();

    a.prologue();
    a.thread_attach(fns);

    // image = mono_image_loaded("Assembly-CSharp")
    a.mov_imm(1, at(OFF_STR_IMAGE) as u64);
    a.mov_imm(0, fns.image_loaded as u64);
    a.call_rax();
    a.test_rax();
    jumps.push(a.jz_placeholder());
    a.mov_rr(3, 0); // rbx = image，之后一直用它

    // klass = mono_class_from_name(image, "", class)
    a.mov_rr(1, 3);
    a.mov_imm(2, at(OFF_STR_NS) as u64);
    a.mov_imm(8, at(OFF_STR_CLASS) as u64);
    a.mov_imm(0, fns.class_from_name as u64);
    a.call_rax();
    a.test_rax();
    jumps.push(a.jz_placeholder());
    a.mov_rr(3, 0); // rbx = klass —— 后面每一步都要用它
    a.store_to(at(OFF_SLOT_KLASS), 3);

    // field = mono_class_get_field_from_name(klass, field)
    a.mov_rr(1, 3);
    a.mov_imm(2, at(OFF_STR_FIELD) as u64);
    a.mov_imm(0, fns.class_get_field_from_name as u64);
    a.call_rax();
    a.test_rax();
    jumps.push(a.jz_placeholder());

    // offset = mono_field_get_offset(field)
    a.mov_rr(1, 0);
    a.mov_imm(0, fns.field_get_offset as u64);
    a.call_rax();
    a.store_to(at(OFF_SLOT_OFFSET), 0);

    // vtable = mono_class_vtable(mono_get_root_domain(), klass)
    a.mov_imm(0, fns.get_root_domain as u64);
    a.call_rax();
    a.mov_rr(1, 0); // rcx = domain
    a.mov_rr(2, 3); // rdx = klass
    a.mov_imm(0, fns.class_vtable as u64);
    a.call_rax();
    a.test_rax();
    jumps.push(a.jz_placeholder());
    a.mov_rr(3, 0); // rbx = vtable
    a.store_to(at(OFF_SLOT_VTABLE), 3);

    // static = mono_vtable_get_static_field_data(vtable)
    a.mov_rr(1, 3);
    a.mov_imm(0, fns.vtable_get_static_field_data as u64);
    a.call_rax();
    a.store_to(at(OFF_SLOT_STATIC), 0);

    // 任何一步失败都跳到收尾，绝不拿空指针去调下一个 mono 函数
    let guard_target = a.code.len();
    for &j in &jumps {
        a.patch_jz_to(j, guard_target);
    }

    a.thread_detach(fns);
    a.epilogue();

    Stub {
        code: a.code,
        jumps,
        guard_target,
    }
}

/// 只读查询：定位 `class.field`，拿到字段偏移、类的 vtable 和静态字段数据区。
///
/// 不做任何执行游戏逻辑的调用。stub 内部每一步之后都有
/// `test rax, rax / jz` 守卫，某步返回 NULL 就立刻跳到收尾。
pub fn resolve_field(
    proc: &Process,
    fns: &MonoFunctions,
    class: &str,
    field: &str,
) -> Result<FieldLayout, String> {
    let block = RemoteBlock::new(proc)?;

    block.put_cstr(OFF_STR_IMAGE, "Assembly-CSharp")?;
    block.put_cstr(OFF_STR_NS, "")?;
    block.put_cstr(OFF_STR_CLASS, class)?;
    block.put_cstr(OFF_STR_FIELD, field)?;
    for off in [
        OFF_SLOT_KLASS,
        OFF_SLOT_OFFSET,
        OFF_SLOT_VTABLE,
        OFF_SLOT_STATIC,
    ] {
        block.clear(off)?;
    }

    let stub = build_resolve_code(fns, block.base);
    block.put(OFF_CODE, &stub.code)?;
    proc.run_remote(block.at(OFF_CODE), 10_000)?;

    Ok(FieldLayout {
        klass: block.get_u64(OFF_SLOT_KLASS).unwrap_or(0),
        // 偏移是 int32，mono 返回时高 32 位可能是脏的，只取低 32 位。
        offset: block.get_u64(OFF_SLOT_OFFSET).unwrap_or(0) as u32 as i32,
        vtable: block.get_u64(OFF_SLOT_VTABLE).unwrap_or(0),
        static_data: block.get_u64(OFF_SLOT_STATIC).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prologue_epilogue_are_symmetric() {
        let mut a = Asm::new();
        a.prologue();
        a.epilogue();
        assert_eq!(
            a.code,
            vec![
                0x53, 0x57, 0x48, 0x83, 0xEC, 0x28, // push rbx; push rdi; sub rsp,0x28
                0x48, 0x83, 0xC4, 0x28, // add rsp,0x28
                0x5F, 0x5B, // pop rdi; pop rbx
                0x48, 0x31, 0xC0, // xor rax,rax
                0xC3, // ret
            ]
        );
    }

    #[test]
    fn mov_imm_encoding() {
        let mut a = Asm::new();
        a.mov_imm(1, 0x1122_3344_5566_7788);
        assert_eq!(
            a.code,
            vec![0x48, 0xB9, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
        );

        let mut a = Asm::new();
        a.mov_imm(8, 2); // r8 需要 REX.B
        assert_eq!(a.code[0..2], [0x49, 0xB8]);
        assert_eq!(a.code.len(), 10);
    }

    #[test]
    fn mov_rr_and_store_encoding() {
        // mov rcx, rax
        let mut a = Asm::new();
        a.mov_rr(1, 0);
        assert_eq!(a.code, vec![0x48, 0x89, 0xC1]);

        // mov rbx, rax
        let mut a = Asm::new();
        a.mov_rr(3, 0);
        assert_eq!(a.code, vec![0x48, 0x89, 0xC3]);

        // mov rdx, rbx
        let mut a = Asm::new();
        a.mov_rr(2, 3);
        assert_eq!(a.code, vec![0x48, 0x89, 0xDA]);

        // mov rdi, rax
        let mut a = Asm::new();
        a.mov_rr(7, 0);
        assert_eq!(a.code, vec![0x48, 0x89, 0xC7]);

        // mov [rcx], rbx
        let mut a = Asm::new();
        a.mov_store(1, 3);
        assert_eq!(a.code, vec![0x48, 0x89, 0x19]);
    }

    #[test]
    fn xor_and_call_encoding() {
        let mut a = Asm::new();
        a.xor_rr(2, 2); // xor rdx, rdx
        assert_eq!(a.code, vec![0x48, 0x31, 0xD2]);

        let mut a = Asm::new();
        a.xor_rr(9, 9); // xor r9, r9 需要 REX.R + REX.B
        assert_eq!(a.code, vec![0x4D, 0x31, 0xC9]);

        let mut a = Asm::new();
        a.call_rax();
        assert_eq!(a.code, vec![0xFF, 0xD0]);
    }

    /// `jz` 编码必须是 rel32（`0F 84`），并且回填后正好落在目标位置。
    #[test]
    fn jz_uses_rel32_and_patches_correctly() {
        let mut a = Asm::new();
        a.test_rax();
        let j = a.jz_placeholder();
        assert_eq!(&a.code[j - 2..j], &[0x0F, 0x84], "应当是 rel32 的 jz");
        a.bs(&[0x90, 0x90, 0x90]); // 三个 nop 作为「被跳过的部分」

        let target = a.code.len();
        a.patch_jz_to(j, target);

        let disp = i32::from_le_bytes(a.code[j..j + 4].try_into().unwrap());
        assert_eq!(j as i64 + 4 + disp as i64, target as i64);
    }

    /// 多个 `jz` 回填到同一位置时互不干扰。
    #[test]
    fn multiple_jz_patches_all_target_same_spot() {
        let mut a = Asm::new();
        let j1 = a.jz_placeholder();
        a.bs(&[0x90, 0x90]);
        let j2 = a.jz_placeholder();
        a.bs(&[0x90]);
        let j3 = a.jz_placeholder();

        let target = a.code.len();
        for j in [j1, j2, j3] {
            a.patch_jz_to(j, target);
        }

        for j in [j1, j2, j3] {
            let disp = i32::from_le_bytes(a.code[j..j + 4].try_into().unwrap());
            assert_eq!(j as i64 + 4 + disp as i64, target as i64);
        }
    }

    fn fake_fns() -> MonoFunctions {
        MonoFunctions {
            get_root_domain: 0x1000,
            thread_attach: 0x1010,
            thread_detach: 0x1020,
            image_loaded: 0x1030,
            class_from_name: 0x1040,
            class_get_field_from_name: 0x1050,
            field_get_offset: 0x1060,
            class_vtable: 0x1070,
            vtable_get_static_field_data: 0x1080,
        }
    }

    /// 所有守卫都必须跳到同一个收尾位置。
    #[test]
    fn resolve_stub_guards_land_on_the_epilogue() {
        let stub = build_resolve_code(&fake_fns(), 0x1_0000_0000);
        assert!(!stub.jumps.is_empty());

        for &j in &stub.jumps {
            assert_eq!(&stub.code[j - 2..j], &[0x0F, 0x84]);
            let disp = i32::from_le_bytes(stub.code[j..j + 4].try_into().unwrap());
            let target = j as i64 + 4 + disp as i64;
            assert_eq!(
                target, stub.guard_target as i64,
                "守卫没有跳到统一的收尾位置"
            );
        }
        assert!(
            stub.guard_target < stub.code.len(),
            "收尾位置应当在代码末尾之前"
        );
    }

    /// 第一个守卫要跳过的距离远超 rel8 的 ±127。
    ///
    /// 这条就是这个 bug 的回归测试：早先用 rel8 编码，
    /// `assert!` 在后台线程里 panic，界面卡死在「处理中」。
    #[test]
    fn resolve_stub_guard_span_exceeds_rel8_range() {
        let stub = build_resolve_code(&fake_fns(), 0x1_0000_0000);
        let first = stub.jumps[0];
        let span = stub.guard_target - (first + 4);
        println!("第一个守卫需要跳过 {span} 字节（rel8 上限 127）");
        assert!(
            span > i8::MAX as usize,
            "守卫跨度只有 {span} 字节；虽然这里断言的是「超过 rel8」，\
             但若真降到 127 以内，说明 stub 结构变了，rel32 的必要性要重新评估"
        );
    }

    /// 序言 + 尾声中 rsp 的净变化必须为零，且调用点满足 16 字节对齐。
    #[test]
    fn stack_is_balanced_and_aligned() {
        // 序言里减的 0x28 = 40 字节，容得下 Win64 要求的 32 字节影子空间。
        // 这条不写成断言——两边都是常量，断言恒真，没有检查价值。

        // 进入 stub 时 rsp ≡ 8 (mod 16)
        let mut rsp: i64 = 8;
        rsp -= 8; // push rbx
        rsp -= 8; // push rdi
        rsp -= 0x28; // sub rsp, 0x28
        assert_eq!(rsp % 16, 0, "call 之前 rsp 必须 16 字节对齐");

        rsp += 0x28;
        rsp += 8;
        rsp += 8;
        assert_eq!(rsp, 8, "收尾后必须回到进入时的栈位置");
    }
}
