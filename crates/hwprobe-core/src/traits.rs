//! 平台接口层:多操作系统预留的核心抽象。
//!
//! - [`Provider`]:一个硬件类别一个实现(CPU / GPU / 磁盘 …)。
//! - [`PlatformBackend`]:整个平台的 provider 集合,Windows 与 Linux 各自实现。
//!
//! 设计约束:
//! - provider 必须是 Send + Sync(refresh 会在后台轮询线程上被调用);
//! - probe() 只在初始化时调用一次,失败只影响该类别,不影响其他 provider;
//! - refresh() 只允许更新动态传感器的值,必须无 panic、有限时(不依赖外部超时)。

use crate::model::{Category, DeviceData, Reading, Status};
use std::time::Duration;

pub trait Provider: Send + Sync {
    /// 诊断用实现名,如 "win-memory"。
    fn describe(&self) -> &str;

    fn category(&self) -> Category;

    /// 枚举设备与传感器清单(静态信息也在此填好)。
    /// 返回 Err 表示该类别整体不可用,会被记录并跳过。
    fn probe(&self) -> Result<Vec<DeviceData>, String>;

    /// 动态刷新周期提示。静态-only 的 provider 返回大值即可。
    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    /// 刷新动态传感器值。实现通过 DeviceData::set_reading 按 code 写值。
    fn refresh(&self, _devices: &mut [DeviceData]) {}
}

pub trait PlatformBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// 返回该平台全部 provider(按类别顺序)。
    fn providers(&self) -> Vec<std::sync::Arc<dyn Provider>>;

    /// 可选:平台级初始化(如驱动加载)。失败返回 Err,后端仍可降级运行。
    fn init(&self) -> Result<(), String> {
        Ok(())
    }

    fn shutdown(&self) {}
}

/// 便捷函数:构造一个成功读值。
pub fn ok(value: crate::model::Value) -> Reading {
    Reading::now(value)
}

/// provider 内部常用的降级读值。
pub fn failed(status: Status, vtype: crate::model::ValueType) -> Reading {
    Reading::failed(status, vtype)
}
