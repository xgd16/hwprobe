//! Linux 后端(预留骨架)。
//!
//! 实现计划(接口已稳定,按 Windows 后端的模式逐类落地):
//! - CPU:      /proc/cpuinfo、/proc/stat、/sys/class/thermal、coretemp/msr 模块
//! - Memory:   /proc/meminfo、/sys/firmware/dmi/tables/DMI(Type 17)
//! - GPU:      /sys/class/drm、NVIDIA NVML、AMD sysfs(hwmon)
//! - Disk:     /sys/block、NVMe /sys/class/nvme/*/health、smartctl 数据
//! - Network:  /sys/class/net、getifaddrs
//! - Battery:  /sys/class/power_supply
//! - 主板:     /sys/class/hwmon(it87/nct6775 内核模块)
//!
//! 当前版本返回 NotSupported,保证接口层可编译可链接。

use crate::model::{Category, DeviceData};
use crate::traits::{Provider, PlatformBackend};
use std::sync::Arc;

pub struct LinuxBackend;

impl LinuxBackend {
    pub fn new() -> LinuxBackend {
        LinuxBackend
    }
}

pub struct StubProvider {
    category: Category,
}

impl Provider for StubProvider {
    fn describe(&self) -> &str {
        "linux-stub"
    }

    fn category(&self) -> Category {
        self.category
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        Err(format!(
            "linux 平台的 {} 类别尚未实现(预留接口)",
            self.category.name()
        ))
    }
}

impl PlatformBackend for LinuxBackend {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn providers(&self) -> Vec<Arc<dyn Provider>> {
        Category::ALL
            .iter()
            .map(|c| {
                let p: Arc<dyn Provider> = Arc::new(StubProvider { category: *c });
                p
            })
            .collect()
    }
}
