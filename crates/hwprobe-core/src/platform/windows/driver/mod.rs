//! 内核驱动增强层:硬件访问原语(MSR / IO 端口 / PCI 配置 / AMD SMN)。
//!
//! 提供两个可互换后端,统一抽象为 [`HwAccess`] trait:
//!
//! - **PawnIO**(优先):Microsoft 签名的通用内核驱动,通过沙箱模块
//!   (IntelMSR / AMDFamily17 / LpcIO,字节码随本 crate 分发)执行特权操作。
//!   驱动本体未被列入易受攻击驱动黑名单,**在 HVCI(内存完整性)开启的
//!   系统上同样可用**。
//! - **WinRing0**(回退):LibreHardwareMonitor/OpenHardwareMonitor 分发的
//!   经典驱动(MPL-2.0),在未安装 PawnIO 且 HVCI 关闭的系统上工作。
//!
//! 两者都需要管理员权限。任何后端不可用(未安装/被策略拦截/非管理员)时
//! `access()` 返回 None,依赖本层的传感器标记 NO_DRIVER,其余功能不受影响。
//!
//! SuperIO(ITE 芯片)传感器读取还依赖 [`HwAccess::superio_begin`] /
//! [`HwAccess::superio_scan_bars`] 两个钩子:PawnIO 后端借此选择 SuperIO
//! 配置端口槽位并在模块内放行 EC BAR 端口;WinRing0 后端为空操作。

#![cfg(target_os = "windows")]

use crate::logbuf;
use std::sync::OnceLock;

mod pawnio;
mod setup;
mod superio_impl;

pub mod acpi_ec;
pub use acpi_ec::AcpiEc;
pub use setup::{install_pawnio, pawnio_installed};
pub use superio_impl::{
    dram_power_supported, plan_sensors, read_package_power_limits, refresh_cpu_enhanced,
    refresh_dram_power, refresh_gt_power, refresh_motherboard,
};

// ---------------- WinRing0 IOCTL 定义(OLS_TYPE = 40000) ----------------

const OLS_TYPE: u32 = 40000;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;

const fn ctl_code(dev: u32, func: u32, method: u32, access: u32) -> u32 {
    (dev << 16) | (access << 14) | (func << 2) | method
}

const IOCTL_OLS_READ_MSR: u32 = ctl_code(OLS_TYPE, 0x821, METHOD_BUFFERED, FILE_ANY_ACCESS);
#[allow(dead_code)]
const IOCTL_OLS_WRITE_MSR: u32 = ctl_code(OLS_TYPE, 0x822, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_OLS_READ_IO_PORT_BYTE: u32 =
    ctl_code(OLS_TYPE, 0x833, METHOD_BUFFERED, FILE_READ_ACCESS);
const IOCTL_OLS_WRITE_IO_PORT_BYTE: u32 =
    ctl_code(OLS_TYPE, 0x836, METHOD_BUFFERED, FILE_WRITE_ACCESS);
const IOCTL_OLS_READ_PCI_CONFIG: u32 =
    ctl_code(OLS_TYPE, 0x851, METHOD_BUFFERED, FILE_READ_ACCESS);
const IOCTL_OLS_WRITE_PCI_CONFIG: u32 =
    ctl_code(OLS_TYPE, 0x852, METHOD_BUFFERED, FILE_WRITE_ACCESS);

#[repr(C, packed)]
struct WriteIoPortInput {
    port: u32,
    value: u8,
}

#[repr(C, packed)]
struct PciConfigInput {
    pci_address: u32,
    reg_address: u32,
}

#[repr(C, packed)]
struct WritePciConfigInput {
    pci_address: u32,
    reg_address: u32,
    value: u32,
}

// ---------------- 统一硬件访问接口 ----------------

pub trait HwAccess: Send + Sync {
    /// 后端名(日志/诊断用)。
    fn name(&self) -> &'static str;

    /// 读 MSR(当前线程亲和性所在核心)。
    fn read_msr(&self, index: u32) -> Option<u64>;

    /// 先设置线程亲和性到指定逻辑核,再读 MSR,最后恢复。
    /// 仅支持处理器组 0(前 64 个逻辑核);超出范围返回 None。
    fn read_msr_on_core(&self, index: u32, core: u32) -> Option<u64> {
        if core >= 64 {
            return None;
        }
        unsafe {
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
            let mask = 1usize << core;
            let prev = SetThreadAffinityMask(GetCurrentThread(), mask);
            if prev == 0 {
                return None;
            }
            // panic 安全:无论 read_msr 是否展开,Drop 都会恢复原亲和性,
            // 避免 poller 线程带着钉死在单核的亲和性继续跑
            struct Restore(usize);
            impl Drop for Restore {
                fn drop(&mut self) {
                    unsafe {
                        SetThreadAffinityMask(GetCurrentThread(), self.0);
                    }
                }
            }
            let _restore = Restore(prev);
            self.read_msr(index)
        }
    }

    fn read_io_port_byte(&self, port: u16) -> Option<u8>;
    fn write_io_port_byte(&self, port: u16, value: u8);

    /// ACPI EC 端口原语(仅 0x62 数据口 / 0x66 命令状态口有意义)。
    /// 与 [`Self::read_io_port_byte`] 分开的原因:PawnIO 的 LpcIO 模块有端口
    /// 白名单,放行的只有 SuperIO 配置口与 EC BAR;ACPI EC 必须经专用的
    /// LpcACPIEC 模块(白名单 0x62/0x66)。默认实现 = 通用端口 I/O
    /// (WinRing0 后端直接可用);PawnIO 后端覆写为走 LpcACPIEC 模块。
    fn ec_read_port(&self, port: u16) -> Option<u8> {
        self.read_io_port_byte(port)
    }
    fn ec_write_port(&self, port: u16, value: u8) {
        self.write_io_port_byte(port, value)
    }

    /// PCI 配置空间地址:(bus << 8) | (device << 3) | function。
    fn read_pci_config(&self, pci_address: u32, reg_address: u32) -> Option<u32>;
    fn write_pci_config(&self, pci_address: u32, reg_address: u32, value: u32) -> bool;

    /// SMN(System Management Network)间接访问:AMD Zen 温度/功耗。
    fn read_amd_smn(&self, smn_address: u32) -> Option<u32>;

    // ---------------- SMBus(内存 SPD / 温度传感器,可选增强) ----------------

    /// 可用 SMBus 总线数量(0 = 后端不支持或控制器未识别)。
    /// 目前仅 PawnIO 后端实现(SmbusI801 / SmbusPIIX4 / SmbusNCT6793 模块)。
    fn smbus_bus_count(&self) -> usize {
        0
    }

    /// SMBus 总线名(日志/诊断用)。
    fn smbus_bus_name(&self, _bus: usize) -> &'static str {
        "?"
    }

    /// 一次 SMBus 传输。`read=true` 时返回读到的字节/字(SMBus 标准字节序,
    /// 低字节在前);写操作成功返回 Some(0)。
    /// `protocol`:0=QUICK 1=BYTE 2=BYTE_DATA 3=WORD_DATA。
    fn smbus_xfer(
        &self,
        _bus: usize,
        _addr: u8,
        _read: bool,
        _command: u8,
        _protocol: u8,
        _data: u16,
    ) -> Option<u16> {
        None
    }

    /// SuperIO 流程钩子:探测某个槽位(0 = 0x2E/0x2F,1 = 0x4E/0x4F)前调用。
    /// PawnIO 后端借此执行 `ioctl_select_slot`;默认空操作。
    fn superio_begin(&self, _slot: u8) -> bool {
        true
    }

    /// SuperIO 流程钩子:配置态内、芯片 ID 有效后调用。
    /// PawnIO 后端借此执行 `ioctl_find_bars`(放行 EC BAR 端口);默认空操作。
    fn superio_scan_bars(&self) -> bool {
        true
    }
}

// ---------------- WinRing0 后端 ----------------

pub struct KernelDriver {
    handle: windows::Win32::Foundation::HANDLE,
}

unsafe impl Send for KernelDriver {}
unsafe impl Sync for KernelDriver {}

static DRIVER: OnceLock<Option<Box<dyn HwAccess>>> = OnceLock::new();
const SERVICE_NAME: &str = "R0hwprobe";
const DEVICE_NAME: &str = "WinRing0_1_2_0";

/// 驱动可用则返回统一访问接口。provider 在 probe/refresh 时以此判断传感器状态。
pub fn access() -> Option<&'static dyn HwAccess> {
    DRIVER.get().and_then(|o| o.as_deref())
}

/// 尝试加载驱动后端(需要管理员)。进程内只尝试一次,结果缓存。
/// 依次尝试 PawnIO(优先,HVCI 兼容)与 WinRing0(回退)。
pub fn try_init() -> Result<(), String> {
    if let Some(res) = DRIVER.get() {
        return match res {
            Some(d) => {
                let _ = d;
                Ok(())
            }
            None => Err("驱动加载失败(已尝试过,结果已缓存)".into()),
        };
    }

    let result: Result<Box<dyn HwAccess>, String> = match pawnio::PawnIoBackend::open() {
        Ok(b) => {
            logbuf::info(format!("PawnIO 后端就绪({})", b.name()));
            Ok(Box::new(b))
        }
        Err(e_pawn) => {
            logbuf::warn(format!("PawnIO 不可用: {e_pawn};尝试 WinRing0 回退"));
            match KernelDriver::open_winring0() {
                Ok(k) => {
                    logbuf::info("WinRing0 驱动就绪");
                    Ok(Box::new(k) as Box<dyn HwAccess>)
                }
                Err(e_win) => Err(format!("PawnIO: {e_pawn}; WinRing0: {e_win}")),
            }
        }
    };

    let ok = result.is_ok();
    let cached: Option<Box<dyn HwAccess>> = match result {
        Ok(d) => Some(d),
        Err(e) => {
            logbuf::warn(format!(
                "驱动增强层不可用,驱动级传感器将标记 NO_DRIVER: {e}"
            ));
            None
        }
    };
    let _ = DRIVER.set(cached);
    if ok {
        Ok(())
    } else {
        Err("驱动增强层不可用".into())
    }
}

impl KernelDriver {
    /// 加载 WinRing0:先直接尝试打开已运行的设备(服务已装/驱动已加载,
    /// 释放文件被锁的升级场景),失败再释放驱动文件 → 安装/启动服务 → 打开。
    fn open_winring0() -> Result<KernelDriver, String> {
        if let Ok(d) = Self::open_device() {
            return Ok(d);
        }
        let sys_path = Self::extract_driver()?;
        install_service(&sys_path)?;
        Self::open_device()
    }

    fn open_device() -> Result<KernelDriver, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let device_path: Vec<u16> = format!("\\\\.\\{DEVICE_NAME}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(device_path.as_ptr()),
                (GENERIC_READ.0 | GENERIC_WRITE.0) as u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .map_err(|e| format!("打开设备 \\\\.\\{DEVICE_NAME} 失败: {e}"))?
        };

        Ok(KernelDriver { handle })
    }

    fn extract_driver() -> Result<String, String> {
        const SYS: &[u8] = include_bytes!("../../../../assets/WinRing0x64.sys");
        let dir = std::env::temp_dir().join("hwprobe");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
        let path = dir.join("WinRing0x64.sys");
        let need_write = match std::fs::metadata(&path) {
            Ok(m) => m.len() as usize != SYS.len(),
            Err(_) => true,
        };
        if need_write {
            std::fs::write(&path, SYS).map_err(|e| format!("写出驱动文件失败: {e}"))?;
        }
        Ok(path.to_string_lossy().to_string())
    }
}

pub fn is_available() -> bool {
    access().is_some()
}

fn install_service(sys_path: &str) -> Result<(), String> {
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::System::Services::*;

    unsafe {
        let manager = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("OpenSCManager 失败(需要管理员权限): {e}"))?;

        let name_w = wide(SERVICE_NAME);
        let path_w = wide(sys_path);

        let service = match CreateServiceW(
            manager,
            PCWSTR(name_w.as_ptr()),
            PCWSTR(name_w.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_KERNEL_DRIVER,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(path_w.as_ptr()),
            None,
            None,
            None,
            None,
            None,
        ) {
            Ok(s) => s,
            Err(e) if e.code() == HRESULT(0x8007_0431u32 as i32) /* ERROR_SERVICE_EXISTS */ => {
                match OpenServiceW(manager, PCWSTR(name_w.as_ptr()), SERVICE_ALL_ACCESS) {
                    Ok(s) => s,
                    Err(e) => {
                        CloseServiceHandle(manager).ok();
                        return Err(format!("OpenService 失败: {e}"));
                    }
                }
            }
            Err(e) => {
                CloseServiceHandle(manager).ok();
                return Err(format!("CreateService 失败: {e}"));
            }
        };

        if let Err(e) = StartServiceW(service, None) {
            // 已经在运行不算失败
            const ERROR_SERVICE_ALREADY_RUNNING: HRESULT = HRESULT(0x8007_0420u32 as i32);
            if e.code() != ERROR_SERVICE_ALREADY_RUNNING {
                CloseServiceHandle(service).ok();
                CloseServiceHandle(manager).ok();
                return Err(format!("StartService 失败: {e}"));
            }
        }
        CloseServiceHandle(service).ok();
        CloseServiceHandle(manager).ok();
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl HwAccess for KernelDriver {
    fn name(&self) -> &'static str {
        "WinRing0"
    }

    fn read_msr(&self, index: u32) -> Option<u64> {
        unsafe {
            let mut out: u64 = 0;
            let mut ret = 0u32;
            let ok = windows::Win32::System::IO::DeviceIoControl(
                self.handle,
                IOCTL_OLS_READ_MSR,
                Some(&index as *const u32 as *const _),
                4,
                Some(&mut out as *mut u64 as *mut _),
                8,
                Some(&mut ret),
                None,
            )
            .is_ok();
            ok.then_some(out)
        }
    }

    fn read_io_port_byte(&self, port: u16) -> Option<u8> {
        unsafe {
            let mut out: u32 = 0;
            let mut ret = 0u32;
            let ok = windows::Win32::System::IO::DeviceIoControl(
                self.handle,
                IOCTL_OLS_READ_IO_PORT_BYTE,
                Some(&(port as u32) as *const u32 as *const _),
                4,
                Some(&mut out as *mut u32 as *mut _),
                4,
                Some(&mut ret),
                None,
            )
            .is_ok();
            ok.then_some((out & 0xFF) as u8)
        }
    }

    fn write_io_port_byte(&self, port: u16, value: u8) {
        unsafe {
            let input = WriteIoPortInput {
                port: port as u32,
                value,
            };
            let mut ret = 0u32;
            let _ = windows::Win32::System::IO::DeviceIoControl(
                self.handle,
                IOCTL_OLS_WRITE_IO_PORT_BYTE,
                Some(&input as *const WriteIoPortInput as *const _),
                std::mem::size_of::<WriteIoPortInput>() as u32,
                None,
                0,
                Some(&mut ret),
                None,
            );
        }
    }

    fn read_pci_config(&self, pci_address: u32, reg_address: u32) -> Option<u32> {
        if reg_address & 3 != 0 {
            return None;
        }
        unsafe {
            let input = PciConfigInput {
                pci_address,
                reg_address,
            };
            let mut out: u32 = 0;
            let mut ret = 0u32;
            let ok = windows::Win32::System::IO::DeviceIoControl(
                self.handle,
                IOCTL_OLS_READ_PCI_CONFIG,
                Some(&input as *const PciConfigInput as *const _),
                std::mem::size_of::<PciConfigInput>() as u32,
                Some(&mut out as *mut u32 as *mut _),
                4,
                Some(&mut ret),
                None,
            )
            .is_ok();
            ok.then_some(out)
        }
    }

    fn write_pci_config(&self, pci_address: u32, reg_address: u32, value: u32) -> bool {
        if reg_address & 3 != 0 {
            return false;
        }
        unsafe {
            let input = WritePciConfigInput {
                pci_address,
                reg_address,
                value,
            };
            let mut ret = 0u32;
            windows::Win32::System::IO::DeviceIoControl(
                self.handle,
                IOCTL_OLS_WRITE_PCI_CONFIG,
                Some(&input as *const WritePciConfigInput as *const _),
                std::mem::size_of::<WritePciConfigInput>() as u32,
                None,
                0,
                Some(&mut ret),
                None,
            )
            .is_ok()
        }
    }

    /// SMN 通过 bus 0 device 0 function 0 的 0x60(地址)/0x64(数据)寄存器间接访问。
    fn read_amd_smn(&self, smn_address: u32) -> Option<u32> {
        const PCI_ADDR: u32 = 0x0000_0000; // bus 0, device 0, function 0
        const SMN_INDEX: u32 = 0x60;
        const SMN_DATA: u32 = 0x64;
        if !self.write_pci_config(PCI_ADDR, SMN_INDEX, smn_address) {
            return None;
        }
        self.read_pci_config(PCI_ADDR, SMN_DATA)
    }
}
