//! 小工具函数。

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 把字符串安全地拷进定长缓冲区(截断 + 保证 NUL 结尾),用于 FFI 结构体。
pub fn copy_to_fixed(dst: &mut [u8], src: &str) {
    let bytes = src.as_bytes();
    let max = dst.len().saturating_sub(1); // 预留 NUL
    let n = bytes.len().min(max);
    dst[..n].copy_from_slice(&bytes[..n]);
    dst[n] = 0;
}

/// Windows 惯用的 u64 → MB 换算。
pub fn bytes_to_mb(v: u64) -> f64 {
    v as f64 / (1024.0 * 1024.0)
}

pub fn bytes_to_gb(v: u64) -> f64 {
    v as f64 / (1024.0 * 1024.0 * 1024.0)
}
