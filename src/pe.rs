//! 最小化的 PE 导出表解析。
//!
//! 只需要一个能力：给定 DLL 文件内容，列出「导出名 -> RVA」。
//! RVA 与加载基址无关，所以对磁盘上的 DLL 解析一次，
//! 再加上该 DLL 在目标进程中的基址，就得到函数在目标进程里的绝对地址。

use std::collections::HashMap;

fn u16_at(d: &[u8], off: usize) -> Result<u16, String> {
    d.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| format!("读取 u16 越界 @0x{off:X}"))
}

fn u32_at(d: &[u8], off: usize) -> Result<u32, String> {
    d.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("读取 u32 越界 @0x{off:X}"))
}

/// 解析导出表，返回 `导出名 -> RVA`。
pub fn parse_exports(data: &[u8]) -> Result<HashMap<String, u32>, String> {
    if data.len() < 0x40 || &data[0..2] != b"MZ" {
        return Err("不是 PE 文件（缺少 MZ 头）".into());
    }

    let pe = u32_at(data, 0x3C)? as usize;
    if data.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err("不是 PE 文件（缺少 PE 签名）".into());
    }

    let n_sections = u16_at(data, pe + 6)? as usize;
    let opt_size = u16_at(data, pe + 20)? as usize;
    let opt_off = pe + 24;
    let magic = u16_at(data, opt_off)?;

    // PE32+ 的数据目录在可选头偏移 112 处，PE32 在 96 处。
    let dd_off = opt_off + if magic == 0x20B { 112 } else { 96 };
    let export_dir_rva = u32_at(data, dd_off)?;
    if export_dir_rva == 0 {
        return Err("该 DLL 没有导出表".into());
    }

    // 段表项字段顺序: Name(8) VirtualSize VirtualAddress SizeOfRawData PointerToRawData
    let mut sections = Vec::with_capacity(n_sections);
    for i in 0..n_sections {
        let o = opt_off + opt_size + i * 40;
        let vsize = u32_at(data, o + 8)? as usize;
        let vaddr = u32_at(data, o + 12)? as usize;
        let rsize = u32_at(data, o + 16)? as usize;
        let raw = u32_at(data, o + 20)? as usize;
        sections.push((vaddr, vsize.max(rsize), raw));
    }

    let rva_to_off = |rva: usize| -> Option<usize> {
        sections
            .iter()
            .find(|(vaddr, size, _)| rva >= *vaddr && rva < vaddr + size)
            .map(|(vaddr, _, raw)| raw + (rva - vaddr))
    };

    let dir = rva_to_off(export_dir_rva as usize).ok_or("导出表 RVA 不在任何段内")?;
    let n_func = u32_at(data, dir + 20)? as usize;
    let n_name = u32_at(data, dir + 24)? as usize;
    let a_func = u32_at(data, dir + 28)? as usize;
    let a_name = u32_at(data, dir + 32)? as usize;
    let a_ord = u32_at(data, dir + 36)? as usize;

    // 这三张表里存的都是 RVA，必须各自再转一次文件偏移。
    // 曾经在这里把 RVA 直接当文件偏移用，结果差 0xC00，读出满屏垃圾。
    let func_off = rva_to_off(a_func).ok_or("函数地址表越界")?;
    let name_off = rva_to_off(a_name).ok_or("函数名表越界")?;
    let ord_off = rva_to_off(a_ord).ok_or("序号表越界")?;

    let mut exports = HashMap::with_capacity(n_name);
    for i in 0..n_name {
        let Ok(name_rva) = u32_at(data, name_off + i * 4) else {
            continue;
        };
        let Some(s) = rva_to_off(name_rva as usize) else {
            continue;
        };
        let end = data[s..]
            .iter()
            .take(256)
            .position(|&c| c == 0)
            .map(|p| s + p);
        let Some(end) = end else { continue };

        let Ok(name) = std::str::from_utf8(&data[s..end]) else {
            continue;
        };
        let Ok(ordinal) = u16_at(data, ord_off + i * 2) else {
            continue;
        };
        if (ordinal as usize) < n_func
            && let Ok(rva) = u32_at(data, func_off + ordinal as usize * 4)
        {
            exports.insert(name.to_owned(), rva);
        }
    }

    if exports.is_empty() {
        return Err("导出表解析结果为空".into());
    }
    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 游戏自带的 mono.dll 路径。没装游戏时这些测试自动跳过。
    fn mono_dll() -> Option<Vec<u8>> {
        const PATHS: [&str; 3] = [
            r"D:\ProgramFiles\steam\steamapps\common\Broforce\Broforce_beta_Data\Mono\EmbedRuntime\mono.dll",
            r"C:\Program Files (x86)\Steam\steamapps\common\Broforce\Broforce_beta_Data\Mono\EmbedRuntime\mono.dll",
            r"E:\SteamLibrary\steamapps\common\Broforce\Broforce_beta_Data\Mono\EmbedRuntime\mono.dll",
        ];
        PATHS.iter().find_map(|p| std::fs::read(p).ok())
    }

    #[test]
    fn rejects_non_pe() {
        assert!(parse_exports(b"not a pe file at all").is_err());
    }

    #[test]
    fn parses_real_mono_dll() {
        let Some(data) = mono_dll() else {
            eprintln!("没找到 mono.dll，跳过");
            return;
        };

        let exports = parse_exports(&data).expect("解析 mono.dll 导出表");
        // 这个 DLL 实测有 806 个导出，留点余量。
        assert!(exports.len() > 700, "导出数量异常: {}", exports.len());

        // 这几个 RVA 是实测值，用来确保「RVA 转文件偏移」那一步没写错。
        // 一旦这里对不上，说明又在某张表上漏转了文件偏移。
        assert_eq!(exports.get("mono_runtime_invoke").copied(), Some(0x789BC));
        assert_eq!(exports.get("mono_get_root_domain").copied(), Some(0x33454));
        assert_eq!(exports.get("mono_class_from_name").copied(), Some(0x26FBC));
        assert_eq!(exports.get("mono_thread_attach").copied(), Some(0xA42A0));
        assert_eq!(exports.get("mono_thread_detach").copied(), Some(0xA446C));
        assert_eq!(exports.get("mono_image_loaded").copied(), Some(0x461B8));
        assert_eq!(exports.get("mono_object_unbox").copied(), Some(0x796B0));
        assert_eq!(
            exports.get("mono_class_get_method_from_name").copied(),
            Some(0x27444)
        );
    }
}
