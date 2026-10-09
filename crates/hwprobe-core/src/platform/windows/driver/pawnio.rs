//! PawnIO 后端:签名驱动 + 沙箱字节码模块。
//!
//! PawnIO(https://pawnio.eu,由 namazso 开发)是 Microsoft 签名的通用内核
//! 驱动,应用通过设备 `\Device\PawnIO` 加载编译好的 Pawn 模块(本 crate
//! 分发 7 个官方模块的 `.bin`,LGPL-2.1,来源 namazso/PawnIO.Modules):
//!
//! - `IntelMSR` / `AMDFamily17`:MSR 与 AMD SMN(按 CPU 厂商二选一,必需);
//! - `LpcIO`:SuperIO 端口 I/O(主板风扇/电压/温度);
//! - `LpcACPIEC`:ACPI EC 端口白名单(仅 0x62/0x66);
//! - `SmbusI801` / `SmbusPIIX4` / `SmbusNCT6793`:SMBus 控制器(SPD / 内存温度)。
//!
//! 各模块按名调用其中的 `ioctl_*` 函数;除 MSR 模块外加载失败都只记日志,
//! 对应传感器降级而非整体不可用。
//!
//! 设备访问协议(与 LibreHardwareMonitor 的 PawnIo.cs 一致):
//! - `IOCTL_PIO_LOAD_BINARY`(0x821):输入为模块字节码;
//! - `IOCTL_PIO_EXECUTE_FN`(0x841):输入 = 32 字节函数名(NUL 填充)+ u64
//!   参数数组,输出 = u64 数组。
//!
//! 模块内的 MSR 白名单与我们的需求精确对应(DTS 0x19C / TjMax 0x1A2 /
//! PKG_THERM 0x1B1 / RAPL 0x606+0x611;AMD 侧 `ioctl_read_smn` 走 PCI
//! 0:0.0 的 0x60/0x64,与 WinRing0 实现等价)。

#![cfg(target_os = "windows")]

use super::HwAccess;
use crate::logbuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// 设备路径(与 PawnIOLib 的 `\Device\PawnIO` 对应,GLOBALROOT 形式便于直接 CreateFile)。
const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
/// PawnIO 设备类型码(见其 pawnio_um.h)。
const DEVICE_TYPE: u32 = 41394;
const IOCTL_PIO_LOAD_BINARY: u32 = (DEVICE_TYPE << 16) | (0x821 << 2);
const IOCTL_PIO_EXECUTE_FN: u32 = (DEVICE_TYPE << 16) | (0x841 << 2);
/// 函数名在输入缓冲中的固定长度。
const FN_NAME_LEN: usize = 32;

/// 官方模块字节码(LGPL-2.1,编译自 namazso/PawnIO.Modules;
/// 二进制取自 LibreHardwareMonitor 0.9.6 的 Resources/PawnIo)。
const INTEL_MSR_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/IntelMSR.bin");
const AMD_FAMILY17_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/AMDFamily17.bin");
const LPCIO_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/LpcIO.bin");
/// SMBus 控制器模块(内存 SPD / 温度传感器读取通路)。
const SMBUS_I801_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/SmbusI801.bin");
const SMBUS_PIIX4_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/SmbusPIIX4.bin");
const SMBUS_NCT6793_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/SmbusNCT6793.bin");
/// ACPI 嵌入式控制器模块(白名单端口 0x62/0x66,掌机/笔记本 EC 传感器通路)。
const LPC_ACPI_EC_BIN: &[u8] = include_bytes!("../../../../assets/pawnio/LpcACPIEC.bin");

/// 一个已加载模块的执行器(一个设备句柄 = 一个模块实例)。
struct PawnModule {
    handle: HANDLE,
}

unsafe impl Send for PawnModule {}
unsafe impl Sync for PawnModule {}

impl PawnModule {
    fn open(blob: &[u8], name: &'static str) -> Result<Self, String> {
        let path: Vec<u16> = DEVICE_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ.0 | GENERIC_WRITE.0) as u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .map_err(|e| format!("打开 PawnIO 设备失败(驱动未安装或非管理员): {e}"))?
        };

        let mut ret = 0u32;
        let loaded = unsafe {
            windows::Win32::System::IO::DeviceIoControl(
                handle,
                IOCTL_PIO_LOAD_BINARY,
                Some(blob.as_ptr() as *const _),
                blob.len() as u32,
                None,
                0,
                Some(&mut ret),
                None,
            )
        };
        if let Err(e) = loaded {
            unsafe {
                CloseHandle(handle).ok();
            }
            return Err(format!("加载 PawnIO 模块 {name} 失败: {e}"));
        }
        Ok(Self { handle })
    }

    /// 执行模块函数。`args` 为 u64 输入,返回 `out_len` 个 u64 输出。
    fn execute(&self, func: &str, args: &[u64], out_len: usize) -> Option<Vec<u64>> {
        let mut input = vec![0u8; FN_NAME_LEN + args.len() * 8];
        let name = func.as_bytes();
        let n = name.len().min(FN_NAME_LEN - 1);
        input[..n].copy_from_slice(&name[..n]);
        for (i, a) in args.iter().enumerate() {
            let at = FN_NAME_LEN + i * 8;
            input[at..at + 8].copy_from_slice(&a.to_le_bytes());
        }

        let mut out = vec![0u64; out_len];
        let mut ret = 0u32;
        let ok = unsafe {
            windows::Win32::System::IO::DeviceIoControl(
                self.handle,
                IOCTL_PIO_EXECUTE_FN,
                Some(input.as_ptr() as *const _),
                input.len() as u32,
                if out_len > 0 {
                    Some(out.as_mut_ptr() as *mut _)
                } else {
                    None
                },
                (out_len * 8) as u32,
                Some(&mut ret),
                None,
            )
            .is_ok()
        };
        if !ok {
            return None;
        }
        Some(out)
    }
}

impl Drop for PawnModule {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle).ok();
        }
    }
}

/// 一个已加载 SMBus 模块(每条控制器总线一个实例)。
struct SmbusModule {
    module: PawnModule,
    name: &'static str,
}

/// PawnIO 后端:MSR 模块(按 CPU 厂商二选一)+ 可选 LpcIO / LpcACPIEC 模块
/// + 可用时加载的 SMBus 控制器模块。
pub struct PawnIoBackend {
    msr: PawnModule,
    lpc: Option<PawnModule>,
    acpi_ec: Option<PawnModule>,
    smbus: Vec<SmbusModule>,
}

impl PawnIoBackend {
    /// 打开设备并按 CPU 厂商加载模块。任何一步失败返回 Err(调用方回退 WinRing0)。
    pub fn open() -> Result<Self, String> {
        let vendor = raw_cpuid::CpuId::new()
            .get_vendor_info()
            .map(|v| v.as_str().to_string())
            .unwrap_or_default();

        let (blob, name): (&[u8], &'static str) = if vendor.eq_ignore_ascii_case("GenuineIntel") {
            (INTEL_MSR_BIN, "IntelMSR")
        } else if vendor.eq_ignore_ascii_case("AuthenticAMD") {
            (AMD_FAMILY17_BIN, "AMDFamily17")
        } else {
            return Err(format!("不支持的 CPU 厂商: {vendor}"));
        };

        let msr = PawnModule::open(blob, name)?;

        // 自检:读一个白名单 MSR,确认模块可执行(Intel 0x1A2 / AMD 0xC0010299 能量单位)
        let probe_msr: u32 = if vendor.eq_ignore_ascii_case("GenuineIntel") {
            0x01A2
        } else {
            0xC001_0299
        };
        if msr.execute("ioctl_read_msr", &[probe_msr as u64], 1).is_none() {
            return Err(format!("{name} 模块自检失败(MSR 读取无响应)"));
        }

        let lpc = match PawnModule::open(LPCIO_BIN, "LpcIO") {
            Ok(m) => Some(m),
            Err(e) => {
                logbuf::warn(format!("PawnIO LpcIO 模块加载失败,主板风扇/电压不可用: {e}"));
                None
            }
        };

        // LpcACPIEC:ACPI EC 专用模块(端口白名单仅 0x62/0x66)。
        // 自检:读一次 EC 状态口(0x66)确认模块可执行;读不到内容也算模块就绪,
        // 端口协议层(acpi_ec.rs)再决定 EC 本身是否存在。
        let acpi_ec = match PawnModule::open(LPC_ACPI_EC_BIN, "LpcACPIEC") {
            Ok(m) => {
                if m.execute("ioctl_pio_read", &[0x66], 1).is_some() {
                    Some(m)
                } else {
                    logbuf::warn("PawnIO LpcACPIEC 模块自检失败(状态口无响应),EC 传感器不可用");
                    None
                }
            }
            Err(e) => {
                logbuf::info(format!("PawnIO LpcACPIEC 模块不可用,EC 传感器不可用: {e}"));
                None
            }
        };

        Ok(Self {
            msr,
            lpc,
            acpi_ec,
            smbus: load_smbus_modules(),
        })
    }
}

/// 加载并识别 SMBus 控制器模块。`ioctl_identity` 成功的才计入;
/// 控制器不存在(如 AMD 平台上的 i801)属正常情况,静默跳过。
fn load_smbus_modules() -> Vec<SmbusModule> {
    let mut buses = Vec::new();

    // Intel PCH(i801)
    match PawnModule::open(SMBUS_I801_BIN, "SmbusI801") {
        Ok(m) if m.execute("ioctl_identity", &[], 3).is_some() => {
            buses.push(SmbusModule { module: m, name: "i801" });
        }
        Ok(_) => {}
        Err(e) => logbuf::info(format!("PawnIO SMBus(i801)模块不可用: {e}")),
    }

    // AMD/旧 Intel(PIIX4):两个可选端口,各需独立模块实例
    for (i, port) in [0u64, 1].into_iter().enumerate() {
        match PawnModule::open(SMBUS_PIIX4_BIN, "SmbusPIIX4") {
            Ok(m)
                if m.execute("ioctl_piix4_port_sel", &[port], 1).is_some()
                    && m.execute("ioctl_identity", &[], 3).is_some() =>
            {
                buses.push(SmbusModule { module: m, name: "piix4" });
            }
            Ok(_) => {}
            Err(e) => {
                logbuf::info(format!("PawnIO SMBus(piix4 port {i})模块不可用: {e}"));
                break;
            }
        }
    }

    // Nuvoton NCT6793(部分主板的 SMBus 主控)
    match PawnModule::open(SMBUS_NCT6793_BIN, "SmbusNCT6793") {
        Ok(m) if m.execute("ioctl_identity", &[], 3).is_some() => {
            buses.push(SmbusModule { module: m, name: "nct6793" });
        }
        Ok(_) => {}
        Err(e) => logbuf::info(format!("PawnIO SMBus(nct6793)模块不可用: {e}")),
    }

    if buses.is_empty() {
        logbuf::info("未发现可用的 PawnIO SMBus 控制器模块(内存温度不可用)");
    } else {
        let names: Vec<&str> = buses.iter().map(|b| b.name).collect();
        logbuf::info(format!("PawnIO SMBus 就绪: {}", names.join(", ")));
    }
    buses
}

impl HwAccess for PawnIoBackend {
    fn name(&self) -> &'static str {
        "PawnIO"
    }

    fn read_msr(&self, index: u32) -> Option<u64> {
        self.msr
            .execute("ioctl_read_msr", &[index as u64], 1)
            .map(|v| v[0])
    }

    fn read_io_port_byte(&self, port: u16) -> Option<u8> {
        self.lpc
            .as_ref()?
            .execute("ioctl_pio_inb", &[port as u64], 1)
            .map(|v| (v[0] & 0xFF) as u8)
    }

    fn write_io_port_byte(&self, port: u16, value: u8) {
        if let Some(lpc) = &self.lpc {
            let _ = lpc.execute("ioctl_pio_outb", &[port as u64, value as u64], 0);
        }
    }

    /// ACPI EC 端口经 LpcACPIEC 模块(其白名单仅 0x62/0x66,其余端口被拒绝)。
    fn ec_read_port(&self, port: u16) -> Option<u8> {
        self.acpi_ec
            .as_ref()?
            .execute("ioctl_pio_read", &[port as u64], 1)
            .map(|v| (v[0] & 0xFF) as u8)
    }

    fn ec_write_port(&self, port: u16, value: u8) {
        if let Some(ec) = &self.acpi_ec {
            let _ = ec.execute("ioctl_pio_write", &[port as u64, value as u64], 0);
        }
    }

    /// LpcIO 模块只放行 SuperIO 相关端口,不提供通用 PCI 配置空间访问。
    fn read_pci_config(&self, _pci_address: u32, _reg_address: u32) -> Option<u32> {
        None
    }

    fn write_pci_config(&self, _pci_address: u32, _reg_address: u32, _value: u32) -> bool {
        false
    }

    /// AMD 模块的 `ioctl_read_smn` 在驱动内走 PCI 0:0.0 的 0x60/0x64,
    /// 与 WinRing0 后端的 `read_amd_smn` 实现等价。
    fn read_amd_smn(&self, smn_address: u32) -> Option<u32> {
        self.msr
            .execute("ioctl_read_smn", &[smn_address as u64], 1)
            .map(|v| v[0] as u32)
    }

    fn superio_begin(&self, slot: u8) -> bool {
        match &self.lpc {
            Some(lpc) => lpc
                .execute("ioctl_select_slot", &[slot as u64], 0)
                .is_some(),
            None => false,
        }
    }

    fn superio_scan_bars(&self) -> bool {
        match &self.lpc {
            Some(lpc) => lpc.execute("ioctl_find_bars", &[], 0).is_some(),
            None => false,
        }
    }

    fn smbus_bus_count(&self) -> usize {
        self.smbus.len()
    }

    fn smbus_bus_name(&self, bus: usize) -> &'static str {
        self.smbus.get(bus).map(|b| b.name).unwrap_or("?")
    }

    fn smbus_xfer(
        &self,
        bus: usize,
        addr: u8,
        read: bool,
        command: u8,
        protocol: u8,
        data: u16,
    ) -> Option<u16> {
        let m = self.smbus.get(bus)?;
        // 输入单元:[addr, read/write, command, protocol, data]。
        // NCT6793 模块的 xfer 是固定 9 输入/5 输出单元;i801/PIIX4 为变长。
        let mut args = vec![addr as u64, read as u64, command as u64, protocol as u64, data as u64];
        let out_len = if m.name == "nct6793" {
            args.resize(9, 0);
            5
        } else {
            1
        };
        let out = m.module.execute("ioctl_smbus_xfer", &args, out_len)?;
        Some(out[0] as u16)
    }
}
