//! 驱动增强传感器实现:CPU 温度/功耗(MSR/SMN/RAPL)+ ITE SuperIO(风扇/电压/温度)。
//!
//! 公式与寄存器移植自 LibreHardwareMonitor v0.9.6(MPL-2.0):
//! - IntelCpu.cs / Amd17Cpu.cs:Intel DTS(IA32_THERM_STATUS 0x19C /
//!   TEMPERATURE_TARGET 0x1A2 / PACKAGE_THERM_STATUS 0x1B1)、RAPL(0x606/0x611)、
//!   AMD Zen SMN 0x59800(THM_TCON_CUR_TMP)与 MSRC001_029B。
//! - IT87XX.cs / LpcIO.cs:ITE SuperIO 完整探测流程(进配置态读芯片 ID →
//!   选 LDN 4 取 EC 基址 → 经 BAR 索引寄存器对读取传感器值)与风扇/电压/温度
//!   公式、每芯片型号的电压增益与 16 位风扇计数差异。
//!
//! 硬件访问全部经 [`super::HwAccess`] 抽象,PawnIO 与 WinRing0 后端通用。

#![cfg(target_os = "windows")]

use super::HwAccess;
use crate::model::{DeviceData, Reading, Status, Value, ValueType};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------- Intel ----------------

const IA32_THERM_STATUS: u32 = 0x019C;
const IA32_TEMPERATURE_TARGET: u32 = 0x01A2;
const IA32_PACKAGE_THERM_STATUS: u32 = 0x1B1;
const MSR_RAPL_POWER_UNIT: u32 = 0x606;
const MSR_PKG_ENERGY_STATUS: u32 = 0x611;
/// RAPL DRAM 域能量计数(内存功耗)。客户端 CPU 多数不实现该域,读取恒为 0。
const MSR_DRAM_ENERGY_STATUS: u32 = 0x619;
/// RAPL 包功耗限制(PL1 bits 14:0 / PL2 bits 46:32,使能位 bit15/47)。
const MSR_PKG_POWER_LIMIT: u32 = 0x610;
/// RAPL GT(核显)域:PP1 能量计数与功耗限制。Meteor Lake 等核显平台的
/// 核显功耗由此差分得到(LibreHardwareMonitor IntelIntegratedGpu 同款)。
const MSR_PP1_ENERGY_STATUS: u32 = 0x641;

// ---------------- AMD family 17h/19h ----------------

const F17H_M01H_THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const MSR_PKG_ENERGY_STAT_AMD: u32 = 0xC001_029B;

// ---------------- ITE SuperIO ----------------

const ITE_CHIP_ID_REGISTER: u8 = 0x20;
const ITE_CHIP_VERSION_REGISTER: u8 = 0x22;
const ITE_VENDOR_ID_REGISTER: u8 = 0x58;
const ITE_VENDOR_IDS: [u8; 2] = [0x90, 0x7F];
const ITE_CONFIGURATION_REGISTER: u8 = 0x00;
const BASE_ADDRESS_REGISTER: u8 = 0x60;
const DEVICE_SELECT_REGISTER: u8 = 0x07;
const IT87_ENVIRONMENT_CONTROLLER_LDN: u8 = 0x04;
/// EC BAR 的索引/数据寄存器偏移(读传感器值经此寄存器对)。
const ADDRESS_REGISTER_OFFSET: u16 = 0x05;
const DATA_REGISTER_OFFSET: u16 = 0x06;
const BANK_REGISTER: u8 = 0x06;
const TEMPERATURE_BASE_REG: u8 = 0x29;
const IT87_REG_VIN: [u8; 13] = [
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2F, 0x2C, 0x2D, 0x2E,
];
const FAN_TACHOMETER_REG: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x4C];
const FAN_TACHOMETER_EXT_REG: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x4D];
const FAN_TACHOMETER_REG_ALT: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x93];
const FAN_TACHOMETER_EXT_REG_ALT: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x94];
const FAN_TACHOMETER_16BIT_REGISTER: u8 = 0x0C;
const FAN_TACHOMETER_16BIT_REGISTER_ALT: u8 = 0x0B;
const FAN_TACHOMETER_DIVISOR_REGISTER: u8 = 0x0B;

/// ISA 总线访问串行化(探测与刷新共用;LHM 的 Mutexes.WaitIsaBus 等价物)。
static ISA_BUS: Mutex<()> = Mutex::new(());
/// 探测结果缓存(probe 阶段填充,refresh 阶段复用 EC 基址)。
static ITE: OnceLock<Option<IteChip>> = OnceLock::new();

// ---------------- 能量计数状态 ----------------

/// 一个 RAPL 能量域的差分状态(PKG / DRAM 各自独立计数)。
struct EnergyState {
    last_energy: Option<u32>,
    last_at: Option<Instant>,
}

impl EnergyState {
    const fn new() -> Self {
        EnergyState {
            last_energy: None,
            last_at: None,
        }
    }
}

static ENERGY_PKG: Mutex<EnergyState> = Mutex::new(EnergyState::new());
static ENERGY_DRAM: Mutex<EnergyState> = Mutex::new(EnergyState::new());
static ENERGY_PP1: Mutex<EnergyState> = Mutex::new(EnergyState::new());

/// RAPL 功率单位(MSR 0x606):bits 3:0 = 1/2^su W,bits 12:8 = 1/2^eu J。
fn rapl_units(drv: &dyn HwAccess) -> (f64, f64) {
    match drv.read_msr(MSR_RAPL_POWER_UNIT) {
        Some(v) => {
            let pu = (v & 0xF) as u32;
            let eu = ((v >> 8) & 0x1F) as u32;
            (1.0 / (1u64 << pu) as f64, 1.0 / (1u64 << eu) as f64)
        }
        None => (1.0 / 8.0, 1.0 / 16.0), // SDM 缺省:1/8 W,1/16 J
    }
}

/// 刷新核显(GT/PP1 域)功耗。调用前提:driver::is_available() 且 Intel CPU。
/// 域缺失(MSR 读取失败)时不写值;恒 0 计数按不支持处理。
pub fn refresh_gt_power(dev: &mut DeviceData) {
    let Some(drv) = super::access() else { return };
    let Some(energy) = drv.read_msr(MSR_PP1_ENERGY_STATUS) else {
        // 读取失败 = 平台无 PP1 域(或 MSR 拒读),标不支持而不是永久 Waiting
        dev.set_reading(
            "power_gt_w",
            Reading::failed(Status::NotSupported, ValueType::F64),
        );
        return;
    };
    let e = energy as u32;
    let (_, unit_j) = rapl_units(drv);
    let mut st = ENERGY_PP1.lock().unwrap_or_else(|e| e.into_inner());
    if e == 0 {
        // 恒 0:域存在但不可用(首轮);运行中计数被清零(iGPU 断电进 D3、
        // 平台门控)同样按此处理 —— 重置基线并保留旧值,恢复后重新建立差分。
        // 与 DRAM 域同款判据,防止把清零当 2^32 回绕算出天文数字功率。
        let first = st.last_energy.is_none();
        *st = EnergyState::new();
        if first {
            dev.set_reading(
                "power_gt_w",
                Reading::failed(Status::NotSupported, ValueType::F64),
            );
        }
        return;
    }
    update_energy_and_write(&mut st, dev, "power_gt_w", e, unit_j, Instant::now());
}

/// 包功耗墙(PL1 稳态 / PL2 瞬时,W)。MSR 0x610:PL1 = bits 14:0,
/// PL2 = bits 46:32,单位 = 1/2^pu W。手动手持机(如 MSI Claw)的 TDP 档位
/// 切换直接反映在这两个字段上。
///
/// 注意:bit15 / bit47 是 **CLAMP 使能位**(允许钳到更低),不是"限制生效"
/// 位 —— 大量 Intel 固件 clamp=0 而 PL1/PL2 值完全有效,因此不作判据;
/// 有效性按数值范围判断,PL2 为 0 视为平台未实现该域。
pub fn read_package_power_limits() -> Option<(f64, Option<f64>)> {
    let drv = super::access()?;
    let limit = drv.read_msr(MSR_PKG_POWER_LIMIT)?;
    let (unit_w, _) = rapl_units(drv);
    let pl1 = ((limit & 0x7FFF) as f64) * unit_w;
    let pl2 = (((limit >> 32) & 0x7FFF) as f64) * unit_w;
    if pl1 <= 0.0 || pl1 > 1000.0 || pl2 > 1000.0 {
        return None;
    }
    let pl2 = (pl2 > 0.0).then_some(pl2);
    Some((pl1, pl2))
}

// ---------------- CPU 增强(温度/功耗) ----------------

/// 刷新 CPU 驱动增强传感器:温度(MSR DTS / AMD SMN)与 RAPL 功耗。
/// 调用前提:driver::is_available() == true。core_count 为 probe 声明的
/// 逐核传感器数(WMI 逻辑核数),必须与声明数一致,否则多出的传感器
/// 会永久停在 Waiting。
pub fn refresh_cpu_enhanced(dev: &mut DeviceData, core_count: usize) {
    let Some(drv) = super::access() else { return };
    let cpuid = raw_cpuid::CpuId::new();
    let vendor = cpuid
        .get_vendor_info()
        .map(|v| v.as_str().to_string())
        .unwrap_or_default();

    let now = Instant::now();

    if vendor.eq_ignore_ascii_case("GenuineIntel") {
        // 逐核 DTS:TjMax(MSR 0x1A2 bits 23:16)- deltaT(MSR 0x19C bits 22:16)
        for core in 0u32..core_count.min(64) as u32 {
            if let Some(tj) = drv.read_msr_on_core(IA32_TEMPERATURE_TARGET, core) {
                let tj_max = ((tj >> 16) & 0xFF) as f64;
                if let Some(status) = drv.read_msr_on_core(IA32_THERM_STATUS, core) {
                    if status & 0x8000_0000 != 0 {
                        let delta = ((status >> 16) & 0x7F) as f64;
                        dev.set_reading(
                            &format!("temperature_core_{core}"),
                            Reading::now(Value::F64(tj_max - delta)),
                        );
                    }
                }
            }
        }
        // 封装温度:MSR 0x1B1
        if let Some(tj) = drv.read_msr_on_core(IA32_TEMPERATURE_TARGET, 0) {
            let tj_max = ((tj >> 16) & 0xFF) as f64;
            if let Some(status) = drv.read_msr_on_core(IA32_PACKAGE_THERM_STATUS, 0) {
                if status & 0x8000_0000 != 0 {
                    let delta = ((status >> 16) & 0x7F) as f64;
                    dev.set_reading(
                        "temperature_pkg",
                        Reading::now(Value::F64(tj_max - delta)),
                    );
                }
            }
        }
        // RAPL 封装功耗:能量单位 MSR 0x606 bits 12:8(默认 1/16 J)
        let mut esu = 4u32; // 1/2^4 J
        if let Some(unit) = drv.read_msr(MSR_RAPL_POWER_UNIT) {
            esu = ((unit >> 8) & 0x1F) as u32;
        }
        let unit_j = 1.0 / (1u64 << esu) as f64;
        if let Some(energy) = drv.read_msr(MSR_PKG_ENERGY_STATUS) {
            let e = energy as u32;
            let mut st = ENERGY_PKG.lock().unwrap_or_else(|e| e.into_inner());
            update_energy_and_write(&mut st, dev, "power_pkg", e, unit_j, now);
        }
    } else if vendor.eq_ignore_ascii_case("AuthenticAMD") {
        // AMD Zen:Tctl 经 SMN 0x59800(PawnIO:ioctl_read_smn;WinRing0:PCI 0:0.0 0x60/0x64)
        if let Some(data) = drv.read_amd_smn(F17H_M01H_THM_TCON_CUR_TMP) {
            let offset_flag = data & (1 << 19) != 0;
            // CUR_TEMP [31:21],单位 0.125°C
            let mut t = ((data >> 21) * 125) as f64 * 0.001;
            if offset_flag {
                t -= 49.0;
            }
            dev.set_reading("temperature_tctl", Reading::now(Value::F64(t)));
        }
        // 封装功耗:MSRC001_0299 能量单位固定 15.3µJ(LHM 同款),0xC001029B 计数
        if let Some(energy) = drv.read_msr(MSR_PKG_ENERGY_STAT_AMD) {
            let e = energy as u32;
            let mut st = ENERGY_PKG.lock().unwrap_or_else(|e| e.into_inner());
            update_energy_and_write(&mut st, dev, "power_pkg", e, 15.3e-6, now);
        }
    }
}

/// 刷新内存功耗(RAPL DRAM 域,MSR 0x619)。调用前提:driver::is_available()。
/// 域缺失或驱动不可用时不写值,由 provider 在 probe 阶段决定是否声明该传感器。
pub fn refresh_dram_power(dev: &mut DeviceData) {
    let Some(drv) = super::access() else { return };
    let Some(energy) = drv.read_msr(MSR_DRAM_ENERGY_STATUS) else {
        return;
    };
    let e = energy as u32;
    let mut st = ENERGY_DRAM.lock().unwrap_or_else(|e| e.into_inner());
    // DRAM 域缺失时该 MSR 恒为 0;真实计数自开机起持续累积,不会为 0。
    if e == 0 {
        *st = EnergyState::new();
        drop(st);
        dev.set_reading(
            "power_dram",
            Reading::failed(Status::NotSupported, ValueType::F64),
        );
        return;
    }
    let mut esu = 4u32; // 1/2^4 J
    if let Some(unit) = drv.read_msr(MSR_RAPL_POWER_UNIT) {
        esu = ((unit >> 8) & 0x1F) as u32;
    }
    let unit_j = 1.0 / (1u64 << esu) as f64;
    update_energy_and_write(&mut st, dev, "power_dram", e, unit_j, Instant::now());
}

/// DRAM RAPL 域是否可用:驱动可访问且能量计数非零。
/// 客户端 CPU 多数不实现该域(MSR 读取恒 0),据此决定是否声明内存功耗传感器。
pub fn dram_power_supported() -> bool {
    super::access()
        .and_then(|drv| drv.read_msr(MSR_DRAM_ENERGY_STATUS))
        .map(|v| v as u32 != 0)
        .unwrap_or(false)
}

/// 能量计数差分(带 32 位回绕处理)→ 功率 W。
fn update_energy_and_write(
    st: &mut EnergyState,
    dev: &mut DeviceData,
    code: &str,
    energy: u32,
    unit_j: f64,
    now: Instant,
) {
    let delta_j = match (st.last_energy, st.last_at) {
        (Some(last), Some(at)) => {
            let dt = now.duration_since(at).as_secs_f64();
            if dt < 0.05 {
                return;
            }
            energy_delta(last, energy) as f64 * unit_j / dt
        }
        _ => 0.0,
    };
    st.last_energy = Some(energy);
    st.last_at = Some(now);
    // 量级钳制:合法功率不会超过 1kW;超大值说明计数被清零/读取异常
    // (模 2^32 回绕差分),重置基线丢弃本样本,不输出假值
    if delta_j > 1000.0 {
        return;
    }
    if delta_j > 0.0 {
        dev.set_reading(
            code,
            Reading::now(Value::F64((delta_j * 100.0).round() / 100.0)),
        );
    }
}

/// 32 位能量计数差分;计数回绕时按模 2^32 计算。
fn energy_delta(last: u32, current: u32) -> u32 {
    current.wrapping_sub(last)
}

// ---------------- ITE SuperIO(主板风扇/电压/温度) ----------------

#[derive(Debug, Clone)]
pub struct SensorPlan {
    pub code: String,
    pub name: String,
    pub unit: String,
    pub vtype: ValueType,
}

/// 探测到的 ITE 芯片参数(EC 基址与通道数在 probe 时确定,refresh 复用)。
#[derive(Debug, Clone, Copy)]
struct IteChip {
    chip_id: u16,
    version: u8,
    /// EC 基址(BAR);传感器值经 BAR+5(索引)/ BAR+6(数据)读取。
    bar: u16,
    voltages: usize,
    temperatures: usize,
    fans: usize,
    voltage_gain: f64,
    has_16bit_fan: bool,
    /// 16 位风扇计数模式下被 BIOS 关闭的通道位掩码(bit i = fan i 关闭)。
    fans_disabled: u8,
    /// IT8655E/IT8665E 每次刷新需先选择 bank 0。
    requires_bank_select: bool,
}

/// 芯片型号 → (电压数, 温度数, 风扇数, 电压增益, 16 位风扇计数, bank 选择)。
/// 数据来自 LHM IT87XX.cs 的 switch(0.9.6)。
fn ite_spec(chip_id: u16, version: u8) -> (usize, usize, usize, f64, bool, bool) {
    let gain = match chip_id {
        0x8613 | 0x8620 | 0x8628 | 0x8631 | 0x8638 | 0x8721 | 0x8728 | 0x8771 | 0x8772
        | 0x8686 | 0x8688 | 0x8689 | 0x8696 => 0.012,
        0x8625 | 0x8733 | 0x8795 => 0.011,
        0x8655 | 0x8665 => 0.0109,
        _ => 0.016,
    };
    let has_16bit = (chip_id != 0x8705 || version >= 3) && (chip_id != 0x8712 || version >= 8);
    let (v, t, f, bank) = match chip_id {
        0x8613 => (10, 4, 5, false),
        0x8625 => (7, 3, 6, false),
        0x8628 => (10, 6, 6, false),
        0x8631 | 0x8638 => (9, 2, 2, false),
        0x8665 => (9, 6, 6, true),
        0x8686 => (10, 6, 6, false),
        0x8688 => (11, 6, 6, false),
        0x8689 => (10, 6, 6, false),
        0x8696 => (10, 6, 6, false),
        0x8795 => (9, 3, 3, false),
        0x8655 => (9, 6, 3, true),
        0x8733 => (9, 3, 3, false),
        0x8705 => (9, 3, 3, false),
        0x8620 => (9, 3, 5, false),
        _ => (9, 3, 5, false),
    };
    (v, t, f, gain, has_16bit, bank)
}

/// 芯片 ID → 是否已知型号。未知型号经厂商/配置校验后仍按默认布局读取
/// (ITE 长尾型号较多,例如 Colorful iGame Z490 上的 0x8637)。
fn ite_known_chip(chip_id: u16) -> bool {
    matches!(
        chip_id,
        0x8613 | 0x8620 | 0x8625 | 0x8628 | 0x8631 | 0x8638 | 0x8665 | 0x8655 | 0x8686
            | 0x8688 | 0x8689 | 0x8696 | 0x8705 | 0x8712 | 0x8716 | 0x8718 | 0x8720
            | 0x8721 | 0x8726 | 0x8728 | 0x8771 | 0x8772 | 0x8790 | 0x8733 | 0x8795
    )
}

/// ITE 配置态进入序列(LHM LpcPort.IT87Enter)。
fn it87_enter(drv: &dyn HwAccess, register_port: u16) {
    drv.write_io_port_byte(register_port, 0x87);
    drv.write_io_port_byte(register_port, 0x01);
    drv.write_io_port_byte(register_port, 0x55);
    let last = if register_port == 0x4E { 0xAA } else { 0x55 };
    drv.write_io_port_byte(register_port, last);
}

/// ITE 配置态退出序列;0x4E(副 SuperIO)按惯例不退出。
fn it87_exit(drv: &dyn HwAccess, register_port: u16, value_port: u16) {
    if register_port != 0x4E {
        drv.write_io_port_byte(register_port, 0x02);
        drv.write_io_port_byte(value_port, 0x02);
    }
}

fn config_read_byte(drv: &dyn HwAccess, register_port: u16, value_port: u16, reg: u8) -> u8 {
    drv.write_io_port_byte(register_port, reg);
    drv.read_io_port_byte(value_port).unwrap_or(0xFF)
}

fn config_read_word(drv: &dyn HwAccess, register_port: u16, value_port: u16, reg: u8) -> u16 {
    ((config_read_byte(drv, register_port, value_port, reg) as u16) << 8)
        | config_read_byte(drv, register_port, value_port, reg + 1) as u16
}

/// EC BAR 索引读:写索引到 BAR+5,从 BAR+6 读数(LHM IT87XX.ReadByte)。
fn bar_read(drv: &dyn HwAccess, bar: u16, reg: u8) -> Option<u8> {
    drv.write_io_port_byte(bar + ADDRESS_REGISTER_OFFSET, reg);
    drv.read_io_port_byte(bar + DATA_REGISTER_OFFSET)
}

fn bar_write(drv: &dyn HwAccess, bar: u16, reg: u8, value: u8) {
    drv.write_io_port_byte(bar + ADDRESS_REGISTER_OFFSET, reg);
    drv.write_io_port_byte(bar + DATA_REGISTER_OFFSET, value);
}

/// 探测 ITE 芯片(LHM LpcIO.DetectIT87):配置态读芯片 ID → 选 LDN 4 取 EC
/// 基址(二次读取校验)→ 退出配置态 → 经 BAR 空间校验厂商 ID 与配置位。
fn detect_ite(drv: &dyn HwAccess) -> Option<IteChip> {
    for &(slot, register_port, value_port) in
        &[(0u8, 0x2Eu16, 0x2Fu16), (1u8, 0x4Eu16, 0x4Fu16)]
    {
        if !drv.superio_begin(slot) {
            continue;
        }

        // 0x4E 槽位:先读芯片 ID,已在配置态(非 0xFFFF)时不重复进入
        // (LHM:重复进入会让 IT8792 误报芯片 ID)。
        let already_entered =
            register_port == 0x4E && config_read_word(drv, register_port, value_port, ITE_CHIP_ID_REGISTER) != 0xFFFF;
        if !already_entered {
            it87_enter(drv, register_port);
        }
        let chip_id = config_read_word(drv, register_port, value_port, ITE_CHIP_ID_REGISTER);

        if chip_id == 0 || chip_id == 0xFFFF {
            if !already_entered {
                it87_exit(drv, register_port, value_port);
            }
            continue;
        }

        // PawnIO:放行后续 BAR 端口(模块内扫描 LDN 基址加入白名单);WinRing0 空操作。
        drv.superio_scan_bars();

        // 选 LDN 4(Environment Controller),读 EC 基址并二次校验
        drv.write_io_port_byte(register_port, DEVICE_SELECT_REGISTER);
        drv.write_io_port_byte(value_port, IT87_ENVIRONMENT_CONTROLLER_LDN);
        let address = config_read_word(drv, register_port, value_port, BASE_ADDRESS_REGISTER);
        std::thread::sleep(Duration::from_millis(2));
        let verify = config_read_word(drv, register_port, value_port, BASE_ADDRESS_REGISTER);
        let version =
            config_read_byte(drv, register_port, value_port, ITE_CHIP_VERSION_REGISTER) & 0x0F;
        if !already_entered {
            it87_exit(drv, register_port, value_port);
        }

        if address != verify || address < 0x100 || (address & 0xF007) != 0 {
            continue;
        }

        // BAR 空间校验:厂商 ID ∈ {0x90, 0x7F};配置寄存器 bit 4 必须为 1
        // (IT8655E/IT8665E 例外,LHM 同款判断)。
        let Some(vendor) = bar_read(drv, address, ITE_VENDOR_ID_REGISTER) else {
            continue;
        };
        if !ITE_VENDOR_IDS.contains(&vendor) {
            continue;
        }
        let Some(configuration) = bar_read(drv, address, ITE_CONFIGURATION_REGISTER) else {
            continue;
        };
        if configuration & 0x10 == 0 && chip_id != 0x8655 && chip_id != 0x8665 {
            continue;
        }

        if !ite_known_chip(chip_id) {
            crate::logbuf::info(format!(
                "ITE SuperIO 型号 0x{chip_id:04X} rev 0x{version:X} 不在已知列表,按默认布局读取(9 电压/3 温度/5 风扇)"
            ));
        }

        let (voltages, temperatures, fans, voltage_gain, has_16bit_fan, requires_bank_select) =
            ite_spec(chip_id, version);

        // 16 位风扇计数模式下,读取 BIOS 关闭的通道位掩码(LHM 构造函数)。
        let mut fans_disabled = 0u8;
        if has_16bit_fan {
            if let Some(modes) = bar_read(drv, address, FAN_TACHOMETER_16BIT_REGISTER) {
                if fans >= 5 {
                    if modes & (1 << 4) == 0 {
                        fans_disabled |= 1 << 3;
                    }
                    if modes & (1 << 5) == 0 {
                        fans_disabled |= 1 << 4;
                    }
                }
                if fans >= 6 {
                    if chip_id == 0x8665 {
                        if let Some(alt) =
                            bar_read(drv, address, FAN_TACHOMETER_16BIT_REGISTER_ALT)
                        {
                            if alt & (1 << 3) == 0 {
                                fans_disabled |= 1 << 5;
                            }
                        }
                    } else if modes & (1 << 2) == 0 {
                        fans_disabled |= 1 << 5;
                    }
                }
            }
        }

        return Some(IteChip {
            chip_id,
            version,
            bar: address,
            voltages,
            temperatures,
            fans,
            voltage_gain,
            has_16bit_fan,
            fans_disabled,
            requires_bank_select,
        });
    }
    None
}

/// 探测 ITE SuperIO 芯片并规划传感器清单(probe 阶段调用)。
pub fn plan_sensors() -> Vec<SensorPlan> {
    let Some(drv) = super::access() else {
        return Vec::new();
    };
    let chip = {
        let _guard = ISA_BUS.lock().unwrap_or_else(|e| e.into_inner());
        *ITE.get_or_init(|| {
            let c = detect_ite(drv);
            match &c {
                Some(chip) => crate::logbuf::info(format!(
                    "ITE SuperIO 探测成功: 芯片 0x{:04X} rev 0x{:X}, EC 基址 0x{:04X}",
                    chip.chip_id, chip.version, chip.bar
                )),
                None => crate::logbuf::info("未发现 ITE SuperIO 芯片"),
            }
            c
        })
    };
    let Some(chip) = chip else {
        return Vec::new();
    };

    let mut plans = Vec::new();
    for i in 0..chip.temperatures {
        plans.push(SensorPlan {
            code: format!("temperature_{i}"),
            name: format!("Motherboard Temperature #{i}"),
            unit: "°C".into(),
            vtype: ValueType::F64,
        });
    }
    for i in 0..chip.fans {
        plans.push(SensorPlan {
            code: format!("fan_{i}"),
            name: format!("Fan #{i}"),
            unit: "RPM".into(),
            vtype: ValueType::F64,
        });
    }
    for i in 0..chip.voltages {
        plans.push(SensorPlan {
            code: format!("voltage_{i}"),
            name: format!("Voltage #{i}"),
            unit: "V".into(),
            vtype: ValueType::F64,
        });
    }
    plans
}

/// 刷新主板 ITE 传感器(风扇转速/电压/温度)。
pub fn refresh_motherboard(devices: &mut [DeviceData]) {
    let Some(drv) = super::access() else {
        for d in devices.iter_mut() {
            for s in d.sensors.iter_mut() {
                if s.desc.kind == crate::model::SensorKind::Dynamic {
                    s.current = Reading::failed(Status::NoDriver, ValueType::F64);
                }
            }
        }
        return;
    };
    if devices.is_empty() {
        return;
    }
    let chip = ITE.get().copied().flatten();
    let Some(chip) = chip else { return };
    let dev = &mut devices[0];

    let _guard = ISA_BUS.lock().unwrap_or_else(|e| e.into_inner());

    // IT8655E/IT8665E:换 bank 后 0x10-0xAF 寄存器窗口切换,每次刷新先回 bank 0。
    if chip.requires_bank_select {
        if let Some(v) = bar_read(drv, chip.bar, BANK_REGISTER) {
            bar_write(drv, chip.bar, BANK_REGISTER, (v & 0x9F) | 0x00);
        }
    }

    let bar = chip.bar;

    // 温度:0x29..(有符号字节,0 < v < 127 有效)
    for i in 0..chip.temperatures {
        let raw = bar_read(drv, bar, TEMPERATURE_BASE_REG + i as u8).map(|v| v as i8);
        match raw {
            Some(v) if v > 0 && v < 127 => dev.set_reading(
                &format!("temperature_{i}"),
                Reading::now(Value::F64(v as f64)),
            ),
            _ => dev.set_reading(
                &format!("temperature_{i}"),
                Reading::failed(Status::NotSupported, ValueType::F64),
            ),
        }
    }

    // 风扇转速
    let (regs, ext_regs) = if chip.chip_id == 0x8665 || chip.chip_id == 0x8625 {
        (FAN_TACHOMETER_REG_ALT, FAN_TACHOMETER_EXT_REG_ALT)
    } else {
        (FAN_TACHOMETER_REG, FAN_TACHOMETER_EXT_REG)
    };
    for i in 0..chip.fans {
        if chip.fans_disabled & (1 << i) != 0 {
            dev.set_reading(
                &format!("fan_{i}"),
                Reading::failed(Status::NotSupported, ValueType::F64),
            );
            continue;
        }
        let rpm = if chip.has_16bit_fan {
            let lo = bar_read(drv, bar, regs[i]).map(u16::from);
            let hi = bar_read(drv, bar, ext_regs[i]).map(|v| (v as u16) << 8);
            match (lo, hi) {
                (Some(lo), Some(hi)) => {
                    let count = lo | hi;
                    // LHM 语义:计数 0xFFFF = 停转(无转速脉冲)→ 0 RPM;过小计数视为无效。
                    if count == 0xFFFF {
                        Some(0.0)
                    } else if count > 0x3F {
                        Some(1_350_000.0 / (count as f64 * 2.0))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            let mut divisor = 2u32;
            if i < 2 {
                match bar_read(drv, bar, FAN_TACHOMETER_DIVISOR_REGISTER) {
                    Some(divs) => divisor = 1 << ((divs >> (3 * i)) & 0x7),
                    None => {
                        dev.set_reading(
                            &format!("fan_{i}"),
                            Reading::failed(Status::NotSupported, ValueType::F64),
                        );
                        continue;
                    }
                }
            }
            match bar_read(drv, bar, regs[i]) {
                Some(v) if v == 0xFF => Some(0.0),
                Some(v) if v > 0x3F => Some(1_350_000.0 / (v as f64 * divisor as f64)),
                _ => None,
            }
        };
        match rpm {
            Some(v) => dev.set_reading(&format!("fan_{i}"), Reading::now(Value::F64(v.round()))),
            None => dev.set_reading(
                &format!("fan_{i}"),
                Reading::failed(Status::NotSupported, ValueType::F64),
            ),
        }
    }

    // 电压:IT87_REG_VIN[i] × 芯片增益(未接入主板的通道读 0,标记不支持)
    for i in 0..chip.voltages.min(IT87_REG_VIN.len()) {
        match bar_read(drv, bar, IT87_REG_VIN[i]) {
            Some(raw) if raw > 0 => dev.set_reading(
                &format!("voltage_{i}"),
                Reading::now(Value::F64(
                    (raw as f64 * chip.voltage_gain * 1000.0).round() / 1000.0,
                )),
            ),
            _ => dev.set_reading(
                &format!("voltage_{i}"),
                Reading::failed(Status::NotSupported, ValueType::F64),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::energy_delta;

    #[test]
    fn energy_delta_handles_wraparound() {
        assert_eq!(energy_delta(100, 150), 50);
        assert_eq!(energy_delta(0, 0), 0);
        // 回绕:0xFFFFFFF0 → 0x10 应为 0x20 个计数(模 2^32)
        assert_eq!(energy_delta(0xFFFF_FFF0, 0x10), 0x20);
    }
}
