//! 定位目标进程里的 mono 运行时，并解析出要调用的那几个 C API 的地址。

use std::collections::HashMap;

use crate::pe;
use crate::win32::Process;

/// 目标进程里的 mono.dll，以及它在磁盘上的导出表。
pub struct MonoApi {
    pub base: usize,
    pub dll_path: String,
    exports: HashMap<String, u32>,
}

impl MonoApi {
    /// 在目标进程的模块列表里找到 mono.dll，再读磁盘上同一个文件解析导出。
    ///
    /// 之所以从磁盘解析而不是把地址写死：RVA 与加载基址无关，
    /// 而游戏更新换了 mono.dll 之后 RVA 会变，写死就会失效。
    pub fn locate(proc: &Process) -> Result<Self, String> {
        let module = proc
            .modules()
            .into_iter()
            .find(|m| m.name.eq_ignore_ascii_case("mono.dll"))
            .ok_or("目标进程里没有加载 mono.dll——这个游戏可能不是 Mono 版 Unity")?;

        let data =
            std::fs::read(&module.path).map_err(|e| format!("读取 {} 失败：{e}", module.path))?;
        let exports = pe::parse_exports(&data)?;

        Ok(Self {
            base: module.base,
            dll_path: module.path,
            exports,
        })
    }

    /// 某个导出函数在目标进程中的绝对地址。
    pub fn addr(&self, name: &str) -> Result<usize, String> {
        self.exports
            .get(name)
            .map(|rva| self.base + *rva as usize)
            .ok_or_else(|| format!("mono.dll 没有导出 {name}"))
    }
}

/// 用到的 mono C API 在目标进程中的地址。
///
/// 全部是**只读的元数据查询**。目标地址算出来之后，读写都由我们这边用
/// `ReadProcessMemory` / `WriteProcessMemory` 完成，不在游戏里执行游戏逻辑。
///
/// 这一点是踩过坑才定下来的：早先的做法是远程调用游戏自己的
/// `HeroController.SetLives`，但那个方法会去刷新 HUD，而 Unity 的 API
/// 只能在主线程碰。从一个外部线程调用它，游戏直接以
/// `UnityPlayer.dll +0x81029C` 访问冲突崩掉（实测崩了两次）。
#[derive(Clone, Copy)]
pub struct MonoFunctions {
    pub get_root_domain: usize,
    pub thread_attach: usize,
    pub thread_detach: usize,
    pub image_loaded: usize,
    pub class_from_name: usize,
    pub class_get_field_from_name: usize,
    pub field_get_offset: usize,
    pub class_vtable: usize,
    pub vtable_get_static_field_data: usize,
}

impl MonoFunctions {
    pub fn resolve(api: &MonoApi) -> Result<Self, String> {
        Ok(Self {
            get_root_domain: api.addr("mono_get_root_domain")?,
            thread_attach: api.addr("mono_thread_attach")?,
            thread_detach: api.addr("mono_thread_detach")?,
            image_loaded: api.addr("mono_image_loaded")?,
            class_from_name: api.addr("mono_class_from_name")?,
            class_get_field_from_name: api.addr("mono_class_get_field_from_name")?,
            field_get_offset: api.addr("mono_field_get_offset")?,
            class_vtable: api.addr("mono_class_vtable")?,
            vtable_get_static_field_data: api.addr("mono_vtable_get_static_field_data")?,
        })
    }
}
