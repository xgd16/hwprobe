//! Windows 后端:用户态官方接口为主,内核驱动(PawnIO / WinRing0)为可选增强层。
//!
//! 数据源总览:
//! - SMBIOS:内存条/主板/BIOS 静态识别
//! - WMI:CPU 识别、GPU 识别、磁盘 SMART(SATA 回退源)
//! - Win32 API:内存占用、CPU 频率、网卡统计(GetIfTable2)、电池系统级状态
//! - IOCTL:磁盘识别(Storage Query Property / NVMe 健康日志)、电池详情
//! - DXGI + PDH:GPU 通用利用率/显存、GPU Engine 利用率
//! - NVML(运行时加载):NVIDIA GPU 完整传感器;ADLX(运行时加载):AMD GPU 传感器
//! - IGCL(运行时加载):Intel 核显/独显遥测
//! - WLAN API:WiFi 关联细节(SSID/频段/信道/信号/协商速率)
//! - Windows Sensor API(COM):IMU 加速度计与陀螺仪
//! - PawnIO(优先,HVCI 兼容)/ WinRing0(回退):CPU 温度/功耗、主板风扇/电压、
//!   ACPI EC(掌机)、SMBus(内存 SPD/温度)

pub mod battery;
pub mod cpu;
pub mod device_profile;
pub mod disk;
pub mod display;
pub mod gpu;
pub mod gpu_adlx;
pub mod gpu_igcl;
pub mod gpu_nvml;
pub mod imu;
pub mod memory;
pub mod msi_ec;
pub mod msi_wmi;
pub mod motherboard;
pub mod net;
pub mod pdh;
#[cfg(target_os = "windows")]
pub mod driver;
pub mod smbus;
pub mod wlan;

use crate::traits::{PlatformBackend, Provider};
use std::sync::Arc;

pub struct WindowsBackend;

impl WindowsBackend {
    pub fn new() -> WindowsBackend {
        WindowsBackend
    }
}

impl Default for WindowsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformBackend for WindowsBackend {
    fn name(&self) -> &'static str {
        "windows"
    }

    /// 平台级初始化:可选加载内核驱动增强层(PawnIO 优先,WinRing0 回退)。
    /// 需要管理员权限;失败(非管理员/HVCI 拦截)只记录,不影响用户态功能。
    /// 设置环境变量 HWPROBE_NO_DRIVER 可完全跳过。
    fn init(&self) -> Result<(), String> {
        if std::env::var_os("HWPROBE_NO_DRIVER").is_some() {
            return Err("HWPROBE_NO_DRIVER 已设置,跳过驱动加载".into());
        }
        driver::try_init()
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

// ---------------- SMBIOS 获取 ----------------

/// 通过 GetSystemFirmwareTable('RSMB') 获取并解析 SMBIOS 表。
pub fn fetch_smbios() -> Result<crate::smbios::Smbios, String> {
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, FIRMWARE_TABLE_PROVIDER};

    unsafe {
        let provider = FIRMWARE_TABLE_PROVIDER(u32::from_be_bytes(*b"RSMB"));
        let size = GetSystemFirmwareTable(provider, 0, None);
        if size == 0 {
            return Err("GetSystemFirmwareTable 查询 RSMB 返回 0".into());
        }
        let mut buf = vec![0u8; size as usize];
        let written = GetSystemFirmwareTable(provider, 0, Some(&mut buf));
        if written == 0 || (written as usize) > buf.len() {
            return Err(format!("GetSystemFirmwareTable 读取失败 written={}", written));
        }
        buf.truncate(written as usize);
        // RAW_SMBIOS_DATA: u8 Used, u8 Major, u8 Minor, u8 Revision, u32 Length, data...
        if buf.len() < 8 {
            return Err("RSMB 数据过短".into());
        }
        let table_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let table = &buf[8..buf.len().min(8 + table_len.max(buf.len() - 8))];
        crate::smbios::Smbios::parse(table)
    }
}

// ---------------- WMI 共享连接 ----------------

/// WMI 连接包装:wmi crate 未实现 Send/Sync(原始 COM 指针),
/// 但我们在 MTA(多线程单元)初始化 COM,IWbemServices 本身是自由线程模型,
/// 跨线程调用安全(且本库仅在初始化线程上使用 WMI 做静态探测)。
pub struct SharedWmi(pub wmi::WMIConnection);

unsafe impl Send for SharedWmi {}
unsafe impl Sync for SharedWmi {}

impl std::ops::Deref for SharedWmi {
    type Target = wmi::WMIConnection;
    fn deref(&self) -> &wmi::WMIConnection {
        &self.0
    }
}

/// 全局共享 WMI 连接(懒创建)。
pub fn shared_wmi() -> Result<&'static SharedWmi, String> {
    use std::sync::OnceLock;
    static CONN: OnceLock<Result<SharedWmi, String>> = OnceLock::new();
    wmi_conn_ns(&CONN, "ROOT\\CIMV2")
}

/// root\WMI 命名空间的共享 WMI 连接(SMART 等驱动提供的数据类)。
pub fn shared_wmi_root_wmi() -> Result<&'static SharedWmi, String> {
    use std::sync::OnceLock;
    static CONN: OnceLock<Result<SharedWmi, String>> = OnceLock::new();
    wmi_conn_ns(&CONN, "ROOT\\WMI")
}

fn wmi_conn_ns(
    slot: &'static std::sync::OnceLock<Result<SharedWmi, String>>,
    namespace: &str,
) -> Result<&'static SharedWmi, String> {
    use std::sync::OnceLock;
    match slot.get_or_init(|| {
        let com = wmi::COMLibrary::new().map_err(|e| format!("COM 初始化失败: {e}"))?;
        wmi::WMIConnection::with_namespace_path(namespace, com)
            .map(SharedWmi)
            .map_err(|e| format!("WMI 连接({namespace})失败: {e}"))
    }) {
        Ok(c) => Ok(c),
        Err(e) => Err(e.clone()),
    }
}

/// 便捷的 WMI 结构体宏:声明裸查询用的字段全部 Option 化。
#[macro_export]
macro_rules! wmi_struct {
    ($name:ident, $class:literal, { $($field:ident : $ty:ty),* $(,)? }) => {
        #[allow(non_snake_case, non_camel_case_types)]
        #[derive(serde::Deserialize, Debug, Clone, Default)]
        #[serde(rename = $class)]
        pub struct $name {
            $( pub $field: Option<$ty>, )*
        }
    };
}
