//! 全局运行时选项(跨平台,进程级生效)。
//!
//! 目前只有"占用口径"一项:决定 CPU/GPU 占用类传感器从哪条数据源取值。
//! 传感器清单与编码不随模式变化,只切换取值来源,因此可在任意时刻
//! (包括 `Registry` 构建之前)设置,下一次刷新即生效。

use std::sync::atomic::{AtomicU32, Ordering};

/// 占用口径。数值与 C ABI `hwprobe_set_usage_mode` 的取值严格对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMode {
    /// 0(默认):CPU = 时间差分(GetSystemTimes / NtQuerySystemInformation),
    /// GPU = 厂商 API(NVML / ADLX / IGCL)。纯"忙碌时间占比",与频率无关。
    Standard = 0,
    /// 1:类 Windows 任务管理器。CPU = PDH `% Processor Utility`(频率加权),
    /// GPU = PDH GPU Engine 计数器(按引擎类型聚合,取最忙引擎)。
    TaskManager = 1,
}

static USAGE_MODE: AtomicU32 = AtomicU32::new(UsageMode::Standard as u32);

/// 切换占用口径。未知取值返回 false 且不生效。
pub fn set_usage_mode(mode: u32) -> bool {
    match mode {
        0 => {
            USAGE_MODE.store(UsageMode::Standard as u32, Ordering::Relaxed);
            true
        }
        1 => {
            USAGE_MODE.store(UsageMode::TaskManager as u32, Ordering::Relaxed);
            true
        }
        _ => false,
    }
}

/// 当前占用口径。
pub fn usage_mode() -> UsageMode {
    match USAGE_MODE.load(Ordering::Relaxed) {
        1 => UsageMode::TaskManager,
        _ => UsageMode::Standard,
    }
}
