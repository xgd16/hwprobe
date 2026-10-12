//! macOS 后端:全部传感器走用户态公开接口,无内核驱动增强层。
//!
//! 数据源总览:
//! - mach / sysctl:CPU 核数/型号/缓存、内存容量与占用(`host_statistics64`)、
//!   CPU 总/逐核占用(`host_processor_info` tick 差分)
//! - IOKit Registry:GPU `PerformanceStatistics`(利用率/显存/温度/功耗)、
//!   磁盘 `IOBlockStorageDriver` 统计与识别、电池 `AppleSmartBattery` 属性、
//!   平台识别(`IOPlatformExpertDevice`)、显示器 EDID
//! - AppleSMC user client:温度/功耗/风扇(键位按芯片代际探测,读不到就是读不到)
//! - NVMe / SATA SMART(IOKit user client):盘温度/磨损/写入量(**需要 root**)
//! - CoreWLAN(ObjC runtime 桥):WiFi 关联细节
//! - CoreGraphics / CoreVideo:分辨率/刷新率
//! - IOPowerSources:电池充放状态/预计续航
//!
//! 与 Windows 版的平台差异(语义对齐、来源不同):
//! - 无内核驱动层概念,`init()` 不做任何事;需要特权的不是"装驱动"而是"root 身份",
//!   无权限时按传感器标记 [`Status::NoPermission`](crate::model::Status::NoPermission)。
//! - CPU 逐核实时频率在 Apple Silicon 上没有用户态接口:回落标称值并在传感器名注明,
//!   不冒充实时值。
//! - IMU 仅极老的 Intel Mac(Sudden Motion Sensor)存在;Apple Silicon 如实报不支持。

pub mod battery;
pub mod cpu;
pub mod disk;
pub mod display;
pub mod gpu;
pub mod imu;
pub mod iokit;
pub mod memory;
pub mod motherboard;
pub mod net;
pub mod smc;
pub mod sys;
pub mod wlan;

use crate::traits::{PlatformBackend, Provider};
use std::sync::Arc;

pub struct MacOSBackend;

impl MacOSBackend {
    pub fn new() -> MacOSBackend {
        MacOSBackend
    }
}

impl Default for MacOSBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformBackend for MacOSBackend {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn providers(&self) -> Vec<Arc<dyn Provider>> {
        // 顺序即类别索引顺序,必须与 Category::ALL 一致
        let list: Vec<Arc<dyn Provider>> = vec![
            Arc::new(cpu::CpuProvider::new()),
            Arc::new(memory::MemoryProvider::new()),
            Arc::new(gpu::GpuProvider::new()),
            Arc::new(disk::DiskProvider::new()),
            Arc::new(net::NetProvider::new()),
            Arc::new(battery::BatteryProvider::new()),
            Arc::new(motherboard::MotherboardProvider::new()),
            Arc::new(display::DisplayProvider::new()),
            Arc::new(imu::ImuProvider::new()),
        ];
        list
    }
}
