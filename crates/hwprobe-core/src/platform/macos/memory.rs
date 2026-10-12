//! Memory provider(macOS):物理内存容量与占用。
//!
//! 数据源:
//! - 容量:`sysctl hw.memsize`;
//! - 占用:`host_statistics64(HOST_VM_INFO64)`(单位:页)。"已用/可用"采用
//!   任务监视器口径:used = (active + wired + compressed)、
//!   available = (free + inactive),与 Windows 版的 used/available 语义对齐。
//!
//! 与 Windows 版的差异(如实省略,不猜):
//! - **无 DIMM 设备**:macOS 不向用户态暴露 SMBIOS Type 17(系统报告用的是私有
//!   SPI),Apple Silicon 又全是板载内存 —— 槽位/厂商/序列号/频率无来源,
//!   不声明逐条内存设备;
//! - **无 commit charge**:macOS 没有对应语义的计数器(交换分区 ≠ 提交量),
//!   不声明 `commit_*` 传感器;
//! - **无内存功耗/内存条温度**:AS 的能耗计数走 IOReport 私有接口,SMC 里
//!   没有可靠的内存温度键(实测 M2 Pro 无 `Tm*` 键)。

use super::sys;
use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use std::time::Duration;

pub struct MemoryProvider;

impl MemoryProvider {
    pub fn new() -> MemoryProvider {
        MemoryProvider
    }
}

impl Provider for MemoryProvider {
    fn describe(&self) -> &str {
        "mac-memory"
    }

    fn category(&self) -> Category {
        Category::Memory
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let total_bytes = sys::sysctl_u64("hw.memsize").ok_or("sysctl hw.memsize 不可用")?;
        if total_bytes == 0 {
            return Err("hw.memsize 返回 0".into());
        }
        let total_mb = total_bytes as f64 / (1024.0 * 1024.0);

        let dev = DeviceBuilder::new("memory.system", "Physical Memory")
            .static_f64("total_mb", "Total Memory", "MB", (total_mb as u64) as f64)
            .dynamic("used_mb", "Memory Used", "MB", ValueType::F64)
            .dynamic("available_mb", "Memory Available", "MB", ValueType::F64)
            .dynamic("used_pct", "Memory Usage", "%", ValueType::F64)
            .build();

        Ok(vec![dev])
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let Some(dev) = devices.first_mut() else {
            return;
        };
        let Some(stats) = sys::vm_statistics() else {
            dev.set_reading("used_mb", Reading::failed(Status::Error, ValueType::F64));
            dev.set_reading(
                "available_mb",
                Reading::failed(Status::Error, ValueType::F64),
            );
            dev.set_reading("used_pct", Reading::failed(Status::Error, ValueType::F64));
            return;
        };

        let page = sys::page_size() as f64;
        let total = sys::sysctl_u64("hw.memsize").unwrap_or(0) as f64;
        if total <= 0.0 {
            dev.set_reading("used_mb", Reading::failed(Status::Error, ValueType::F64));
            return;
        }

        // 任务监视器口径:已用 = 活跃 + 内联 + 压缩;可用 = 空闲 + 非活跃
        let used = (stats.active_count as f64
            + stats.wire_count as f64
            + stats.compressor_page_count as f64)
            * page;
        let avail = (stats.free_count as f64 + stats.inactive_count as f64) * page;
        let used_mb = used / (1024.0 * 1024.0);
        let avail_mb = avail / (1024.0 * 1024.0);
        let pct = (used / total * 100.0).clamp(0.0, 100.0);

        dev.set_reading("used_mb", Reading::now(Value::F64(used_mb.round())));
        dev.set_reading("available_mb", Reading::now(Value::F64(avail_mb.round())));
        dev.set_reading(
            "used_pct",
            Reading::now(Value::F64((pct * 10.0).round() / 10.0)),
        );
    }
}
