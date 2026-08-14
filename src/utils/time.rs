//! 时间戳辅助（跨平台，纯逻辑）。
//!
//! 截图文件名需要人类可读的时间戳。标准库无 epoch→年月日 的格式化，
//! 也不引第三方时间库，这里用 Howard Hinnant 的 civil-from-days 算法
//! （见其 `chrono-compatible low-level date algorithms` 文章）手写实现。
//! 注意：输出为 **UTC 时间**（标准库跨平台无法取本地时区偏移，
//! 本地时区显示待 Phase 5 打磨时用平台 API 处理）。

use std::time::{SystemTime, UNIX_EPOCH};

/// 把 Unix epoch 秒数拆解为 `(year, month, day, hour, minute, second)`（UTC）。
///
/// 算法基于公历规则，正确处理闰年与世纪边界（如 2000 / 2400 闰年，
/// 1900 / 2100 平年）。
pub fn civil_from_epoch_secs(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = (rem / 3_600) as u32;
    let minute = ((rem % 3_600) / 60) as u32;
    let second = (rem % 60) as u32;

    // civil_from_days：1970-01-01 对应天数 0
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    (year, m, d, hour, minute, second)
}

/// 当前 UTC 时间的 `YYYYMMDD_HHMMSS` 时间戳字符串（用于截图文件名）。
pub fn timestamp_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d, hh, mm, ss) = civil_from_epoch_secs(secs);
    format!("{y:04}{m:02}{d:02}_{hh:02}{mm:02}{ss:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_1970_utc() {
        assert_eq!(civil_from_epoch_secs(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn known_timestamp() {
        // 2026-08-14 00:00:00 UTC = 1786665600
        assert_eq!(
            civil_from_epoch_secs(1_786_665_600),
            (2026, 8, 14, 0, 0, 0)
        );
    }

    #[test]
    fn leap_year_boundaries() {
        // 2000-02-29 与 2000-03-01（闰世纪年）
        assert_eq!(civil_from_epoch_secs(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_epoch_secs(951_868_800), (2000, 3, 1, 0, 0, 0));
        // 2100-03-01（平世纪年，2 月只有 28 天）
        assert_eq!(civil_from_epoch_secs(4_107_542_400), (2100, 3, 1, 0, 0, 0));
    }

    #[test]
    fn timestamp_str_format() {
        let s = timestamp_str();
        assert_eq!(s.len(), 15, "{s}");
        assert_eq!(s.as_bytes()[8], b'_');
        assert!(s.chars().all(|c| c.is_ascii_digit() || c == '_'));
    }
}
