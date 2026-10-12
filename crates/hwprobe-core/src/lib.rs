//! # hwprobe-core
//!
//! 类 HWiNFO 硬件信息库的核心层:数据模型、平台 trait 接口、传感器注册表。
//!
//! - Windows 后端完整实现(`platform::windows`),包括用户态数据源与
//!   可选的内核驱动增强层(PawnIO 优先,WinRing0 回退:CPU 温度 / 功耗 /
//!   SuperIO 风扇电压)。
//! - Linux 后端为预留骨架,trait 接口稳定后按同样模式实现。

pub mod intel_id;
pub mod logbuf;
pub mod options;
pub mod model;
pub mod registry;
pub mod smbios;
pub mod stream;
pub mod traits;
pub mod util;

pub mod platform;

/// C ABI 版本,与 `include/hwprobe.h` 的 `HWPROBE_ABI_VERSION` 宏严格对应。
///
/// 这里是唯一的一份定义:DLL 用它实现 `hwprobe_abi_version()`,CLI 用它打印诊断信息,
/// 头文件里的宏是它的对外镜像 —— 三处不可能各自漂移。
///
/// 演进历史:
/// - `1`:仅拉取模式
/// - `2`:新增订阅推送(`hwprobe_subscribe` / `hwprobe_unsubscribe`)
/// - `3`:新增 `Display`(7)/ `IMU`(8)类别
/// - `4`:新增 `hwprobe_set_usage_mode`(占用口径切换)与 GPU 分引擎传感器 `util_eng_*`
/// - `5`:新增状态码 `HWPROBE_NO_PERMISSION`(权限不足;macOS 上 NVMe SMART / 部分 SMC 键需要 root)
pub const HWPROBE_ABI_VERSION: u32 = 5;

pub use model::{
    Category, DeviceBuilder, DeviceData, DeviceMeta, Reading, SensorData, SensorDesc, SensorKind,
    Status, Value, ValueType, ALL_CATEGORIES, CATEGORY_COUNT,
};
pub use registry::Registry;
pub use stream::{StreamBatch, StreamCallback, StreamItem};
pub use traits::{PlatformBackend, Provider};

/// 按编译目标创建平台后端。
pub fn create_backend() -> Result<Box<dyn PlatformBackend>, String> {
    platform::create_backend()
}
