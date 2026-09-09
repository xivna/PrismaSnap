//! 字体选择（Phase 5，2026-09-09 用户需求）。
//!
//! 两类字体独立配置：
//! - **界面字体**：设置窗口与覆盖层 UI（egui）；
//! - **标注/翻译字体**：文字标注、译文覆盖的导出（CPU）与预览（egui 独立 family）。
//!
//! 空路径 = 系统默认（微软雅黑兜底链路，向后兼容）。字体文件按需读盘 + 进程内
//! 缓存（`Arc<Vec<u8>>`），不进 exe（便携版体积不受影响）；列表来自 Windows
//! 注册表字体项（友好的显示名），失败回退 `C:\Windows\Fonts` 目录扫描。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// 字体条目（设置页下拉列表用）。
#[derive(Debug, Clone, PartialEq)]
pub struct FontChoice {
    /// 显示名（如"微软雅黑"；无注册表名时用文件名去扩展名）。
    pub display: String,
    /// 字体文件绝对路径。
    pub path: String,
}

static INTERFACE_FONT: RwLock<Option<String>> = RwLock::new(None);
static ANNOTATION_FONT: RwLock<Option<String>> = RwLock::new(None);
static BYTES_CACHE: RwLock<Option<HashMap<String, Arc<Vec<u8>>>>> = RwLock::new(None);
static FONT_LIST_CACHE: RwLock<Option<Vec<FontChoice>>> = RwLock::new(None);

/// 设置界面字体（空串/None = 恢复系统默认）。
pub fn set_interface_font(path: Option<String>) {
    *INTERFACE_FONT.write().unwrap() = path.filter(|p| !p.is_empty());
}

/// 设置标注/翻译字体（空串/None = 恢复系统默认）。
pub fn set_annotation_font(path: Option<String>) {
    *ANNOTATION_FONT.write().unwrap() = path.filter(|p| !p.is_empty());
}

/// 当前界面字体路径（空 = 系统默认）。
pub fn interface_font_path() -> Option<String> {
    INTERFACE_FONT.read().unwrap().clone()
}

/// 当前标注/翻译字体路径（空 = 系统默认）。
pub fn annotation_font_path() -> Option<String> {
    ANNOTATION_FONT.read().unwrap().clone()
}

/// 按路径加载字体字节（进程内缓存，预览/导出共享同一份）。
pub fn load_font_bytes(path: &str) -> Option<Arc<Vec<u8>>> {
    if let Some(map) = BYTES_CACHE.read().unwrap().as_ref() {
        if let Some(b) = map.get(path) {
            return Some(b.clone());
        }
    }
    let bytes = std::fs::read(path).ok()?;
    let bytes = Arc::new(bytes);
    BYTES_CACHE
        .write()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(path.to_string(), bytes.clone());
    Some(bytes)
}

/// 粗体变体候选路径（纯字符串规则，单测可跑）：同目录同名嵌入 `bd`/`Bold` 变体
/// （msyh.ttc → msyhbd.ttc）。调用方按存在性取第一个命中的。
pub fn bold_variant_candidates(path: &str) -> Vec<String> {
    let Some((dir_file, ext)) = path.rsplit_once('.') else {
        return Vec::new();
    };
    let Some((dir, file)) = dir_file.rsplit_once('\\') else {
        return Vec::new();
    };
    let stem = file;
    [
        format!("{stem}bd.{ext}"),
        format!("{stem}bd.{ext}").to_uppercase(),
        format!("{stem}-Bold.{ext}"),
        format!("{stem}_Bold.{ext}"),
        format!("{stem} Bold.{ext}"),
        format!("{stem}bold.{ext}"),
    ]
    .into_iter()
    .map(|c| format!("{dir}\\{c}"))
    .collect()
}

/// 粗体变体路径（候选中第一个实际存在的文件；找不到返回 None 由上层模拟加粗）。
pub fn bold_variant_path(path: &str) -> Option<String> {
    bold_variant_candidates(path)
        .into_iter()
        .find(|c| std::path::Path::new(c).is_file())
}

/// 枚举系统字体（Windows 注册表 `HKLM\...\CurrentVersion\Fonts`；结果进程内缓存）。
/// 非 Windows 返回空（设置页据此隐藏字体选项）。
pub fn list_fonts() -> Vec<FontChoice> {
    if let Some(list) = FONT_LIST_CACHE.read().unwrap().as_ref() {
        return list.clone();
    }
    let list = list_fonts_uncached();
    FONT_LIST_CACHE.write().unwrap().replace(list.clone());
    list
}

#[cfg(target_os = "windows")]
fn list_fonts_uncached() -> Vec<FontChoice> {
    match list_fonts_from_registry() {
        Some(v) if !v.is_empty() => v,
        _ => list_fonts_from_dir(),
    }
}

#[cfg(not(target_os = "windows"))]
fn list_fonts_uncached() -> Vec<FontChoice> {
    Vec::new()
}

/// 注册表枚举：值名（如 `微软雅黑 (TrueType)`）→ 数据（`msyh.ttc`）。
#[cfg(target_os = "windows")]
fn list_fonts_from_registry() -> Option<Vec<FontChoice>> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    let mut hkey = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            windows::core::w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts"),
            None,
            KEY_READ,
            &mut hkey,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name = [0u16; 512];
        let mut name_len = name.len() as u32;
        let mut data = [0u16; 256];
        let mut data_len = (data.len() * 2) as u32;
        let mut value_type = Default::default();
        let res = unsafe {
            RegEnumValueW(
                hkey,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut value_type),
                Some(data.as_mut_ptr() as *mut u8),
                Some(&mut data_len),
            )
        };
        if res == ERROR_NO_MORE_ITEMS {
            break;
        }
        if res != ERROR_SUCCESS {
            index += 1;
            if index > 20_000 {
                break;
            }
            continue;
        }
        index += 1;
        // 值名形如 "微软雅黑 (TrueType)"；数据为 REG_SZ 文件名。
        let name_str = String::from_utf16_lossy(&name[..name_len as usize]);
        let file = String::from_utf16_lossy(&data[..(data_len as usize / 2).min(data.len())]);
        let file = file.trim_end_matches('\0').trim().to_string();
        if file.is_empty() || file.to_ascii_lowercase().ends_with(".fon") {
            continue;
        }
        let display = name_str
            .replace(" (TrueType)", "")
            .replace("(TrueType)", "")
            .trim()
            .to_string();
        let path = format!("C:\\Windows\\Fonts\\{}", file.replace('/', "\\"));
        if !std::path::Path::new(&path).is_file() {
            continue;
        }
        out.push(FontChoice { display, path });
    }
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    // 校验可解析 + 去重 + 按显示名排序
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| {
        seen.insert(c.path.clone())
            && std::fs::read(&c.path)
                .map(|b| parseable(&b))
                .unwrap_or(false)
    });
    out.sort_by_key(|a| a.display.to_lowercase());
    Some(out)
}

/// 注册表失败回退：目录扫描（文件名去扩展名做显示名）。
#[cfg(target_os = "windows")]
fn list_fonts_from_dir() -> Vec<FontChoice> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("C:\\Windows\\Fonts") {
        for e in rd.flatten() {
            let p = e.path();
            let ext = p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
            if !matches!(ext.as_deref(), Some("ttf") | Some("ttc") | Some("otf")) {
                continue;
            }
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else { continue };
            let display = name
                .rsplit_once('.')
                .map(|(s, _)| s.to_string())
                .unwrap_or_else(|| name.to_string());
            out.push(FontChoice { display, path: p.to_string_lossy().to_string() });
        }
    }
    out.retain(|c| {
        std::fs::read(&c.path)
            .map(|b| parseable(&b))
            .unwrap_or(false)
    });
    out.sort_by_key(|a| a.display.to_lowercase());
    out
}

/// 文件能否被 ab_glyph 解析（ttc 集合取 index 0）。
fn parseable(bytes: &[u8]) -> bool {
    ab_glyph::FontRef::try_from_slice_and_index(bytes, 0).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_variant_heuristic() {
        // 纯字符串规则（文件存在性由 bold_variant_path 过滤，Linux 单测不依赖磁盘）
        assert_eq!(
            bold_variant_candidates("C:\\Fonts\\foo\\msyh.ttc")[0],
            "C:\\Fonts\\foo\\msyhbd.ttc"
        );
        assert_eq!(
            bold_variant_candidates("C:\\Fonts\\SourceHanSans-Regular.otf")[0],
            "C:\\Fonts\\SourceHanSans-Regularbd.otf"
        );
        assert!(bold_variant_candidates("noext").is_empty());
        assert!(bold_variant_path("C:\\nonexistent_dir\\msyh.ttc").is_none());
    }

    #[test]
    fn set_get_roundtrip() {
        set_annotation_font(Some("C:\\x\\a.ttf".into()));
        assert_eq!(annotation_font_path().as_deref(), Some("C:\\x\\a.ttf"));
        set_annotation_font(Some(String::new()));
        assert_eq!(annotation_font_path(), None);
        set_annotation_font(None);
    }
}
