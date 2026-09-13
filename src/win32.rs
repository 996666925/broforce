//! 对 Windows 进程 / 内存 / 远程线程 API 的最小封装。

use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, PROCESSENTRY32W,
    Process32FirstW, Process32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAllocEx, VirtualFreeEx,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeProcess, OpenProcess, PROCESS_CREATE_THREAD,
    PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE,
    WaitForSingleObject,
};

/// 目标进程是 32 位时需要，和 PROCESS_QUERY_LIMITED_INFORMATION 同值。
const PROCESS_WOW64: u32 = 0x0800;

/// 最近一次 Win32 调用的错误码。
///
/// 必须在失败的那个调用之后**紧接着**取，中间不能穿插别的 Win32 调用。
/// 之前只报「CreateRemoteThread 失败」而不带错误码，排查时等于没说。
pub fn last_error() -> String {
    let code = unsafe { GetLastError() };
    format!("Win32 错误码 {code}")
}

/// 进程里的一个模块。
#[derive(Clone, Debug)]
pub struct ModuleInfo {
    pub name: String,
    pub base: usize,
    pub path: String,
}

/// 把 UTF-16 缓冲区转成 `String`（截断到第一个 NUL）。
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 按可执行文件名查找进程，返回所有匹配的 PID。
pub fn find_pids(exe_name: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() {
        return pids;
    }

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        if wide_to_string(&entry.szExeFile).eq_ignore_ascii_case(exe_name) {
            pids.push(entry.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }

    unsafe { CloseHandle(snapshot) };
    pids
}

/// 列出进程加载的所有模块。
pub fn list_modules(pid: u32) -> Vec<ModuleInfo> {
    let mut out = Vec::new();
    // 带上 TH32CS_SNAPMODULE32 (0x10)，目标为 32 位进程时需要。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | 0x10, pid) };
    if snapshot.is_null() {
        return out;
    }

    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;

    let mut ok = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        out.push(ModuleInfo {
            name: wide_to_string(&entry.szModule),
            base: entry.modBaseAddr as usize,
            path: wide_to_string(&entry.szExePath),
        });
        ok = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
    }

    unsafe { CloseHandle(snapshot) };
    out
}

/// 一个已打开、且具备读 / 写 / 远程建线程权限的进程句柄。
///
/// 句柄本身是指针、不满足 `Send`，但这里用到的 API 都是线程安全的，
/// 所以手动实现 `Send` / `Sync`。
pub struct Process {
    handle: HANDLE,
    pub pid: u32,
}

unsafe impl Send for Process {}
unsafe impl Sync for Process {}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Process {
    pub fn open(pid: u32) -> Result<Self, String> {
        let access = PROCESS_QUERY_INFORMATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE
            | PROCESS_VM_OPERATION
            | PROCESS_CREATE_THREAD
            | PROCESS_WOW64;

        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            return Err(format!(
                "OpenProcess 失败（PID {pid}）。如果游戏是以管理员身份启动的，本程序也需要以管理员身份运行。"
            ));
        }
        Ok(Self { handle, pid })
    }

    pub fn modules(&self) -> Vec<ModuleInfo> {
        list_modules(self.pid)
    }

    /// 进程是否还在运行。
    pub fn is_alive(&self) -> bool {
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(self.handle, &mut code) };
        ok != 0 && code == STILL_ACTIVE as u32
    }

    /// 读一段内存，全部读到才算成功。
    pub fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        let mut read: usize = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                &mut read,
            )
        };
        ok != 0 && read == buf.len()
    }

    /// 读一个 64 位值（指针）。
    pub fn read_u64(&self, addr: usize) -> Option<u64> {
        let mut buf = [0u8; 8];
        self.read(addr, &mut buf).then(|| u64::from_ne_bytes(buf))
    }

    /// 读一个 32 位整数。
    pub fn read_i32(&self, addr: usize) -> Option<i32> {
        let mut buf = [0u8; 4];
        self.read(addr, &mut buf).then(|| i32::from_ne_bytes(buf))
    }

    /// 写一个 32 位整数。
    pub fn write_i32(&self, addr: usize, value: i32) -> bool {
        self.write(addr, &value.to_ne_bytes())
    }

    /// 往目标进程写一段内存。
    pub fn write(&self, addr: usize, buf: &[u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        let mut written: usize = 0;
        let ok = unsafe {
            WriteProcessMemory(
                self.handle,
                addr as *const c_void,
                buf.as_ptr() as *const c_void,
                buf.len(),
                &mut written,
            )
        };
        ok != 0 && written == buf.len()
    }

    /// 在目标进程里申请一块可读可写可执行的内存。
    pub fn alloc(&self, size: usize) -> Option<usize> {
        let p = unsafe {
            VirtualAllocEx(
                self.handle,
                std::ptr::null(),
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        };
        (!p.is_null()).then_some(p as usize)
    }

    /// 释放 `alloc` 申请的内存。
    pub fn free(&self, addr: usize) {
        unsafe { VirtualFreeEx(self.handle, addr as *mut c_void, 0, MEM_RELEASE) };
    }

    /// 在目标进程里从 `entry` 起跑一个线程并等它结束。
    pub fn run_remote(&self, entry: usize, timeout_ms: u32) -> Result<(), String> {
        let thread = unsafe {
            CreateRemoteThread(
                self.handle,
                std::ptr::null(),
                0,
                Some(std::mem::transmute::<
                    usize,
                    unsafe extern "system" fn(*mut c_void) -> u32,
                >(entry)),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
            )
        };
        if thread.is_null() {
            return Err(format!("CreateRemoteThread 失败（{}）", last_error()));
        }

        let wait = unsafe { WaitForSingleObject(thread, timeout_ms) };
        unsafe { CloseHandle(thread) };

        match wait {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(format!("远程线程超时（{timeout_ms} ms）未返回")),
            other => Err(format!("等待远程线程失败，返回码 {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_own_process_by_exe_name() {
        let exe = std::env::current_exe().expect("current_exe");
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert!(find_pids(&name).contains(&std::process::id()));
    }

    #[test]
    fn exe_name_match_is_case_insensitive() {
        let exe = std::env::current_exe().expect("current_exe");
        let name = exe.file_name().unwrap().to_string_lossy().to_uppercase();
        assert!(find_pids(&name).contains(&std::process::id()));
    }

    /// 读写通路 + 远程线程自检：往目标进程写一段「返回 42」的代码并跑它。
    #[test]
    fn reads_writes_and_runs_code_in_own_process() {
        let Ok(proc) = Process::open(std::process::id()) else {
            // 受限环境下打不开自己，跳过而不是误报失败。
            return;
        };
        assert!(proc.is_alive());

        let buf: Vec<i32> = vec![0; 16];
        let addr = buf.as_ptr() as usize;

        assert_eq!(proc.read_u64(addr), Some(0));
        assert!(proc.write(addr, &999i32.to_ne_bytes()));
        // 用 volatile 读，避免编译器把写入当成没发生过。
        assert_eq!(unsafe { std::ptr::read_volatile(buf.as_ptr()) }, 999);

        // mov eax, 42; ret   —— 确认能真的在目标进程里执行代码
        let code = proc.alloc(0x1000).expect("VirtualAllocEx");
        assert!(proc.write(code, &[0xB8, 42, 0, 0, 0, 0xC3]));
        proc.run_remote(code, 5000).expect("远程线程执行");
        proc.free(code);
    }

    #[test]
    fn lists_own_modules() {
        let modules = list_modules(std::process::id());
        assert!(!modules.is_empty(), "应该至少列出自己的主模块");
        assert!(
            modules.iter().any(|m| m.base != 0 && !m.path.is_empty()),
            "模块应当有非零基址和路径"
        );
    }
}
