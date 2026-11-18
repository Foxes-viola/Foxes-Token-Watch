//! 时间相关的小工具：ISO 时间解析、本地时区、哈希。

use std::time::{SystemTime, UNIX_EPOCH};

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;

pub fn now_ms() -> i64 {
    system_time_ms(SystemTime::now())
}

pub fn system_time_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// 本地时区相对 UTC 的偏移（毫秒）。
pub fn local_offset_ms() -> i64 {
    chrono::Local::now().offset().local_minus_utc() as i64 * SECOND
}

/// `ts` 所在本地自然日的 0 点（epoch 毫秒）。
pub fn day_start(ts: i64, offset_ms: i64) -> i64 {
    (ts + offset_ms).div_euclid(DAY) * DAY - offset_ms
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn digits(bytes: &[u8]) -> Option<i64> {
    let mut value = 0i64;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + (byte - b'0') as i64;
    }
    Some(value)
}

/// 解析 `2026-11-10T06:09:53.901Z` 这类 ISO-8601 时间（小数秒和时区偏移可选）。
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    parse_iso_bytes(text.as_bytes())
}

pub fn parse_iso_bytes(bytes: &[u8]) -> Option<i64> {
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;

    let mut pos = 19;
    let mut millis = 0i64;
    if pos < bytes.len() && bytes[pos] == b'.' {
        pos += 1;
        let mut scale = 100;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            millis += (bytes[pos] - b'0') as i64 * scale;
            scale /= 10;
            pos += 1;
        }
    }

    let mut offset_minutes = 0i64;
    if pos < bytes.len() && (bytes[pos] == b'+' || bytes[pos] == b'-') {
        let sign = if bytes[pos] == b'-' { -1 } else { 1 };
        let rest = &bytes[pos + 1..];
        if rest.len() >= 2 {
            let hours = digits(&rest[0..2])?;
            let minutes = if rest.len() >= 5 && rest[2] == b':' {
                digits(&rest[3..5])?
            } else if rest.len() >= 4 {
                digits(&rest[2..4])?
            } else {
                0
            };
            offset_minutes = sign * (hours * 60 + minutes);
        }
    }

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

/// FNV-1a，作去重键用；保证不返回 0（0 留作“不去重”）。
pub fn fnv64(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for part in parts {
        for &byte in *part {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utc_with_millis() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso_ms("2024-02-29T12:34:56.789Z"), Some(1_709_210_096_789));
    }

    #[test]
    fn parses_without_fraction_and_with_offset() {
        assert_eq!(parse_iso_ms("2024-02-29T12:34:56Z"), Some(1_709_210_096_000));
        assert_eq!(parse_iso_ms("2024-02-29T20:34:56+08:00"), Some(1_709_210_096_000));
        assert_eq!(parse_iso_ms("2024-02-29T20:34:56.5+0800"), Some(1_709_210_096_500));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_iso_ms("not a time"), None);
        assert_eq!(parse_iso_ms("2024-02-29"), None);
    }

    #[test]
    fn day_start_respects_offset() {
        let offset = 8 * HOUR;
        // 2024-02-29 20:34:56 +08:00 的当天 0 点 = 2024-02-28T16:00:00Z
        let ts = 1_709_210_096_000;
        assert_eq!(day_start(ts, offset), parse_iso_ms("2024-02-28T16:00:00Z").unwrap());
    }
}
