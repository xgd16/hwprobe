//! Battery provider(macOS):IOPowerSources + AppleSmartBattery(IOKit Registry)。
//!
//! 两个来源,语义与 Windows 版(`platform/windows/battery.rs`)逐 code 对齐:
//! - **IOPowerSources**(`io_kit_sys::ps`):充放状态、电量百分比、预计续航/充满
//!   时间、AC 在线;
//! - **IOKit Registry `AppleSmartBattery`**(缺失时回退 `AppleRawBattery`)属性:
//!   容量(mAh)、电压(mV)、电流(mA)、温度(0.1K)、循环次数、序列号等识别信息。
//!
//! # 单位换算(与 Windows 的 mWh / mW 口径对齐,mac 原生 mAh / mA / mV)
//!
//! - **`rate_mw` = −Amperage(mA) × Voltage(mV) / 1000**。功率 P = U·I,
//!   mV×mA 是 µW 量级,除 1000 才是 mW(实测:12603mV × −1500mA = +18904mW)。
//!   取负号是因为 Apple 约定 **负=放电**,而 Windows `BATTERY_STATUS.Rate` 约定
//!   **正=放电、负=充电**,两平台必须同号同义,故在此翻转。
//! - **`capacity_mwh` = 容量(mAh) × 电压(mV) / 1000**(mV·mAh 是 µWh 量级,
//!   除 1000 得 mWh)。电压基准:动态剩余能量用**当前电压**;静态设计/满充容量
//!   优先 `DesignVoltage`,缺失才退回当前电压。锂电池端电压随电量在标称 ~11.5V
//!   与满充 ~12.6V 之间波动,用当前电压换算的 mWh 因此是**近似值**(偏差可达
//!   ±10% 且随电量变化)—— 同源的 mAh 原值以 `design_capacity_mah` /
//!   `full_charge_capacity_mah` 静态传感器原样保留,换算值与原值并存,互不冒充。
//! - **温度**:`Temperature` 属性单位 0.1K,°C = T/10 − 273.15
//!   (与 Windows batclass 的温度同一口径)。
//! - **时间**:IOPowerSources 的 `Time to Empty` / `Time to Full Charge` 单位是
//!   **分钟**,×60 得秒。0 与 65535(0xFFFF)都是"未知/计算中",不产出估计值。
//!
//! # 键布局的机型差异(实测:MacBook,Apple Silicon)
//!
//! - Intel:DesignCapacity / AppleRawMaxCapacity / AppleRawCurrentCapacity /
//!   Temperature / ManufactureDate / Manufacturer / DesignVoltage 等直接挂在
//!   `AppleSmartBattery` 顶层;
//! - Apple Silicon:mAh 系数值收进嵌套字典 `BatteryData`,顶层 `MaxCapacity` /
//!   `CurrentCapacity` 是**百分比归一值**(MaxCapacity=100),且没有 Temperature /
//!   ManufactureDate 等键 —— 读不到就不声明对应传感器。
//!
//! 取值一律"顶层优先、`BatteryData` 嵌套回退";容量优先取显式的 Raw/Full 键,
//! 避免把百分比归一值当 mAh 参与换算。
//!
//! 台式机(Mac mini / Mac Studio 等)没有任何电池:probe() 返回空清单(不算错误),
//! 与 Windows 版"台式机为空清单"一致。

use super::iokit::{self, CfDict};
use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef};
use core_foundation_sys::dictionary::{
    CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef,
};
use core_foundation_sys::string::{kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringRef};
use io_kit_sys::ps::keys as psk;
use io_kit_sys::ps::power_sources as ps;
use std::ffi::{CStr, CString};
use std::time::Duration;

// ---------------- 来源快照:原值 + 原生单位(探针与 provider 共用) ----------------

/// IOPowerSources 的电池快照。字段保持来源原值,不做单位换算。
#[derive(Debug, Default, Clone)]
pub struct PsSnapshot {
    /// 电源来源名(如 `"InternalBattery-0"`)。
    pub name: Option<String>,
    /// 类型:InternalBattery / UPS(`kIOPSTypeKey`)。
    pub kind: Option<String>,
    /// `kIOPSPowerSourceStateKey`:`"AC Power"` / `"Battery Power"`。
    pub state: Option<String>,
    /// 整机当前供电来源是否交流(`IOPSGetProvidingPowerSourceType`)。
    pub ac_online: Option<bool>,
    pub charging: Option<bool>,
    pub charged: Option<bool>,
    pub present: Option<bool>,
    /// 电量百分比(Current/Max 归一后 ×100,0..=100)。
    pub pct: Option<f64>,
    /// `Time to Empty`,原值单位**分钟**。
    pub time_to_empty_min: Option<i64>,
    /// `Time to Full Charge`,原值单位**分钟**。
    pub time_to_full_min: Option<i64>,
    /// 电压,原值单位 mV。
    pub voltage_mv: Option<i64>,
    /// 电流,原值单位 mA(负=放电)。
    pub current_ma: Option<i64>,
}

/// AppleSmartBattery(或 AppleRawBattery)注册表属性快照,原值 + 原生单位。
#[derive(Debug, Default, Clone)]
pub struct RegSnapshot {
    /// 电池电压(mV,实测量级 ~12600)。
    pub voltage_mv: Option<i64>,
    /// 电流(mA,负=放电):`InstantAmperage` 优先,回退均值 `Amperage`。
    pub amperage_ma: Option<i64>,
    /// 温度原值(单位 0.1K),换算见 [`temperature_10k_to_c`]。
    pub temperature_raw: Option<i64>,
    /// 设计容量(mAh,`DesignCapacity`)。
    pub design_mah: Option<i64>,
    /// 满充容量(mAh,`FullChargeCapacity` / `AppleRawMaxCapacity`)。
    pub full_mah: Option<i64>,
    /// 当前剩余容量(mAh,`AppleRawCurrentCapacity` / `RemainingCapacity`)。
    pub remaining_mah: Option<i64>,
    /// 顶层 `CurrentCapacity`(机型不同:百分比归一值或 mAh,仅作比值兜底)。
    pub top_current: Option<i64>,
    /// 顶层 `MaxCapacity`(百分比归一机型恒为 100)。
    pub top_max: Option<i64>,
    pub cycle_count: Option<i64>,
    /// 设计循环次数(`DesignCycleCount9C`)。
    pub design_cycle_count: Option<i64>,
    /// 序列号:`BatterySerialNumber` 回退 `Serial`。
    pub serial: Option<String>,
    /// 电池计量芯片型号(如 `"bq40z651"`)。
    pub device_name: Option<String>,
    pub manufacturer: Option<String>,
    /// 生产日期原值(SBS 0x1B 编码),解码见 [`decode_manufacture_date`]。
    pub manufacture_date_raw: Option<i64>,
    /// 设计电压(mV,Intel 机型常见)。
    pub design_voltage_mv: Option<i64>,
    pub external_connected: Option<bool>,
    pub fully_charged: Option<bool>,
    pub is_charging: Option<bool>,
    pub battery_installed: Option<bool>,
    pub at_critical: Option<bool>,
}

// ---------------- 读取层 ----------------

/// 临时 CFString 键:用完自动 `CFRelease`。
struct KeyGuard(Option<CFStringRef>);

impl KeyGuard {
    fn raw(&self) -> CFStringRef {
        self.0.unwrap_or(std::ptr::null())
    }
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            // SAFETY: p 来自 CFStringCreateWithCString(Create 语义),只释放一次。
            unsafe { CFRelease(p as CFTypeRef) };
        }
    }
}

/// 由 C 字符串常量造临时 CFString 键(`io_kit_sys::ps::keys` 的常量形态)。
fn cf_key_from_cstr(p: *const std::os::raw::c_char) -> KeyGuard {
    if p.is_null() {
        return KeyGuard(None);
    }
    // SAFETY: p 是 NUL 结尾常量字符串;UTF-8 编码;失败返回 null。
    let s = unsafe { CFStringCreateWithCString(std::ptr::null(), p, kCFStringEncodingUTF8) };
    KeyGuard((!s.is_null()).then_some(s))
}

/// 由 Rust 字符串造临时 CFString 键。
fn cf_key(s: &str) -> KeyGuard {
    let Ok(c) = CString::new(s) else {
        return KeyGuard(None);
    };
    // SAFETY: c 是合法 NUL 结尾字符串;UTF-8 编码;失败返回 null。
    let p =
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) };
    KeyGuard((!p.is_null()).then_some(p))
}

/// 从**借用的** CFDictionary 取值(IOPS 的描述字典是 Get 语义、无所有权,
/// 不能套 [`iokit::CfDict`] 的自动释放包装,故在此直接查)。
fn dict_raw_value(dict: CFDictionaryRef, key: &KeyGuard) -> Option<CFTypeRef> {
    let k = key.raw();
    if dict.is_null() || k.is_null() {
        return None;
    }
    // SAFETY: dict 为有效字典;CFDictionaryGetValue 返回借用指针,不释放。
    let v = unsafe {
        if CFGetTypeID(dict as CFTypeRef) != CFDictionaryGetTypeID() {
            return None;
        }
        CFDictionaryGetValue(dict, k as *const _) as CFTypeRef
    };
    if v.is_null() {
        None
    } else {
        Some(v)
    }
}

/// `io_kit_sys` 的键/值常量是 C 字符串指针,转 String 便于比较与打印。
fn cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: 常量是 NUL 结尾字符串。
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// `Power Source State` 是否为 AC。
fn snap_state_is_ac(state: Option<&str>) -> Option<bool> {
    state.map(|s| s == cstr(psk::kIOPSACPowerValue))
}

/// 读取 IOPowerSources:遍历电源列表,取电池型(InternalBattery 优先,其次 UPS)
/// 来源的描述字典。没有电池型来源返回 None(台式机属正常情况)。
pub fn power_source_snapshot() -> Option<PsSnapshot> {
    // SAFETY: IOPSCopy* 为 Create 语义(+1,本函数负责 CFRelease);
    // IOPSGet* 为 Get 语义(借用,不得释放)。
    unsafe {
        let blob = ps::IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return None;
        }
        let list = ps::IOPSCopyPowerSourcesList(blob);
        let mut best: Option<(i32, PsSnapshot)> = None;
        if !list.is_null() {
            let n = iokit::cf_array_len(list as CFTypeRef).unwrap_or(0);
            for i in 0..n {
                let Some(item) = iokit::cf_array_at(list as CFTypeRef, i) else {
                    continue;
                };
                let desc = ps::IOPSGetPowerSourceDescription(blob, item);
                if desc.is_null() {
                    continue;
                }
                let k_type = cf_key_from_cstr(psk::kIOPSTypeKey);
                let kind = dict_raw_value(desc, &k_type).and_then(iokit::cf_string);
                let rank = match kind.as_deref() {
                    Some(k) if k == cstr(psk::kIOPSInternalBatteryType) => 2,
                    Some(k) if k == cstr(psk::kIOPSUPSType) => 1,
                    _ => 0,
                };
                if rank == 0 {
                    continue;
                }
                let get_i64 = |p: *const std::os::raw::c_char| {
                    let k = cf_key_from_cstr(p);
                    dict_raw_value(desc, &k).and_then(iokit::cf_i64)
                };
                let get_bool = |p: *const std::os::raw::c_char| {
                    let k = cf_key_from_cstr(p);
                    dict_raw_value(desc, &k).and_then(iokit::cf_bool)
                };
                let get_str = |p: *const std::os::raw::c_char| {
                    let k = cf_key_from_cstr(p);
                    dict_raw_value(desc, &k).and_then(iokit::cf_string)
                };
                let state = get_str(psk::kIOPSPowerSourceStateKey);
                let cur = get_i64(psk::kIOPSCurrentCapacityKey);
                let max = get_i64(psk::kIOPSMaxCapacityKey);
                let snap = PsSnapshot {
                    name: get_str(psk::kIOPSNameKey),
                    kind,
                    state,
                    ac_online: None, // 由整机供电来源统一判定(见下)
                    charging: get_bool(psk::kIOPSIsChargingKey),
                    charged: get_bool(psk::kIOPSIsChargedKey),
                    present: get_bool(psk::kIOPSIsPresentKey),
                    pct: match (cur, max) {
                        (Some(c), Some(m)) if c >= 0 && m > 0 => {
                            Some((c as f64 / m as f64 * 100.0).clamp(0.0, 100.0))
                        }
                        _ => None,
                    },
                    time_to_empty_min: get_i64(psk::kIOPSTimeToEmptyKey),
                    time_to_full_min: get_i64(psk::kIOPSTimeToFullChargeKey),
                    voltage_mv: get_i64(psk::kIOPSVoltageKey),
                    current_ma: get_i64(psk::kIOPSCurrentKey),
                };
                if best.as_ref().map_or(true, |(r, _)| rank > *r) {
                    best = Some((rank, snap));
                }
            }
            CFRelease(list as CFTypeRef);
        }

        // 整机供电来源:是否 AC(IOPSGetProvidingPowerSourceType,Get 语义)。
        let providing = ps::IOPSGetProvidingPowerSourceType(blob);
        let ac = if providing.is_null() {
            None
        } else {
            iokit::cf_string(providing as CFTypeRef).map(|s| s == cstr(psk::kIOPSACPowerValue))
        };
        CFRelease(blob as CFTypeRef);

        let (_, mut snap) = best?;
        // 拿不到整机供电来源时,退回该来源自己的 Power Source State。
        snap.ac_online = ac.or_else(|| snap_state_is_ac(snap.state.as_deref()));
        Some(snap)
    }
}

/// 电池注册表属性字典的读取器:顶层优先、`BatteryData` 嵌套回退。
struct Props {
    top: CfDict,
    nested: Option<CFDictionaryRef>,
}

impl Props {
    fn load() -> Option<Props> {
        for class in ["AppleSmartBattery", "AppleRawBattery"] {
            let Some(obj) = iokit::matching_service(class) else {
                continue;
            };
            let Some(top) = iokit::registry_props(&obj) else {
                continue;
            };
            // Apple Silicon 把 mAh 系数值收进嵌套字典 BatteryData。
            let nested = iokit::dict_value(&top, "BatteryData")
                .filter(|v| {
                    // SAFETY: v 是非空 CF 对象;仅做类型判定。
                    unsafe { CFGetTypeID(*v) == CFDictionaryGetTypeID() }
                })
                .map(|v| v as CFDictionaryRef);
            return Some(Props { top, nested });
        }
        None
    }

    /// 顶层 → BatteryData 嵌套,逐键回退。
    fn get(&self, key_name: &str) -> Option<CFTypeRef> {
        iokit::dict_value(&self.top, key_name).or_else(|| {
            let d = self.nested?;
            let k = cf_key(key_name);
            dict_raw_value(d, &k)
        })
    }

    fn i64(&self, key_name: &str) -> Option<i64> {
        self.get(key_name).and_then(iokit::cf_i64)
    }

    fn string(&self, key_name: &str) -> Option<String> {
        self.get(key_name).and_then(iokit::cf_string)
    }

    fn bool(&self, key_name: &str) -> Option<bool> {
        self.get(key_name).and_then(iokit::cf_bool)
    }
}

/// 读取电池注册表属性(无电池服务返回 None)。
pub fn registry_snapshot() -> Option<RegSnapshot> {
    let p = Props::load()?;
    let mut r = RegSnapshot {
        voltage_mv: p.i64("Voltage").or_else(|| p.i64("AppleRawBatteryVoltage")),
        amperage_ma: p.i64("InstantAmperage").or_else(|| p.i64("Amperage")),
        temperature_raw: p.i64("Temperature"),
        // 容量优先取显式的 Raw/Full 键(mAh),避免把百分比归一值当 mAh。
        design_mah: p.i64("DesignCapacity"),
        full_mah: p
            .i64("AppleRawMaxCapacity")
            .or_else(|| p.i64("FullChargeCapacity")),
        remaining_mah: p
            .i64("AppleRawCurrentCapacity")
            .or_else(|| p.i64("RemainingCapacity"))
            .or_else(|| p.i64("TrueRemainingCapacity")),
        top_current: iokit::dict_value(&p.top, "CurrentCapacity").and_then(iokit::cf_i64),
        top_max: iokit::dict_value(&p.top, "MaxCapacity").and_then(iokit::cf_i64),
        cycle_count: p.i64("CycleCount"),
        design_cycle_count: p.i64("DesignCycleCount9C"),
        serial: p
            .string("BatterySerialNumber")
            .or_else(|| p.string("Serial")),
        device_name: p.string("DeviceName"),
        manufacturer: p.string("Manufacturer"),
        manufacture_date_raw: p.i64("ManufactureDate"),
        design_voltage_mv: p.i64("DesignVoltage"),
        external_connected: p.bool("ExternalConnected"),
        fully_charged: p.bool("FullyCharged"),
        is_charging: p.bool("IsCharging"),
        battery_installed: p.bool("BatteryInstalled"),
        at_critical: p.bool("AtCriticalLevel"),
    };
    // 兜底:顶层 Current/Max 本身就是 mAh 的机型(百分比归一机型 Max=100)。
    if r.full_mah.is_none() {
        r.full_mah = r.top_max.filter(|v| *v != 100);
    }
    if r.remaining_mah.is_none() {
        r.remaining_mah = r
            .top_current
            .zip(r.top_max)
            .and_then(|(c, m)| (m != 100).then_some(c));
    }
    Some(r)
}

// ---------------- 纯换算与解码(可单测) ----------------

/// 电池温度:注册表 `Temperature` 单位 0.1K → °C(与 Windows batclass 的
/// IOCTL_BATTERY_INFO_TEMPERATURE 同一口径)。越界视为不支持,不猜值。
fn temperature_10k_to_c(raw: i64) -> Option<f64> {
    if raw == 0 {
        return None;
    }
    let c = raw as f64 / 10.0 - 273.15;
    if (-30.0..120.0).contains(&c) {
        Some((c * 10.0).round() / 10.0)
    } else {
        None
    }
}

/// 容量换算:mAh × mV → mWh。能量 E = U·Q,mV·mAh 是 µWh 量级,除 1000 得 mWh。
/// 电压基准由调用方决定(动态用当前电压,静态优先 DesignVoltage),
/// 换算来源与偏差见模块注释。
fn mah_mv_to_mwh(mah: i64, mv: i64) -> Option<f64> {
    if mah < 0 || mv <= 0 {
        return None;
    }
    Some(mah as f64 * mv as f64 / 1000.0)
}

/// 充放电速率换算:Amperage(mA,负=放电)× Voltage(mV) → mW,并翻转符号以对齐
/// Windows `BATTERY_STATUS.Rate` 的约定(正=放电、负=充电)。
/// mV·mA 是 µW 量级,除 1000 得 mW。
fn amperage_mv_to_rate_mw(amp_ma: i64, mv: i64) -> Option<i64> {
    if mv <= 0 {
        return None;
    }
    Some(-(amp_ma * mv) / 1000)
}

/// IOPowerSources 时间值(分钟)→ 秒。0 / 65535(0xFFFF 哨兵)/ 超过 24 小时
/// 一律视为"未知或计算中",不产出估计值。
fn minutes_to_seconds(min: i64) -> Option<f64> {
    if min > 0 && min <= 24 * 60 {
        Some(min as f64 * 60.0)
    } else {
        None
    }
}

/// SBS(Smart Battery Data)规范 0x1B `ManufactureDate` 编码:
/// day:5 | month:4(1-12)| year:7(自 1980 起)。与 Windows batclass 的
/// IOCTL BatteryManufactureDate 是同一编码。校验不过返回 None,不猜日期。
fn decode_manufacture_date(raw: i64) -> Option<String> {
    if !(0..=0xFFFF).contains(&raw) {
        return None;
    }
    let raw = raw as u32;
    let day = raw & 0x1F;
    let month = (raw >> 5) & 0x0F;
    let year = 1980 + ((raw >> 9) & 0x7F);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1980..=2107).contains(&year) {
        return None;
    }
    Some(format!("{year}-{month:02}-{day:02}"))
}

/// 状态字符串,取值与 Windows 版保持一致(Charging / Discharging / AC Power
/// (±Fully Charged) / Idle,临界时追加 " (Critical)"),便于跨平台同码同义。
/// 供电与充电状态都读不到时返回 None(不编造状态)。
fn state_string(
    ac: Option<bool>,
    charging: Option<bool>,
    charged: Option<bool>,
    critical: Option<bool>,
    pct: Option<f64>,
) -> Option<String> {
    let mut state = if charging == Some(true) {
        "Charging"
    } else if ac == Some(false) {
        "Discharging"
    } else if ac == Some(true) {
        if charged == Some(true) || matches!(pct, Some(p) if p >= 99.0) {
            "AC Power (Fully Charged)"
        } else {
            "AC Power"
        }
    } else if charging == Some(false) {
        "Idle"
    } else {
        return None;
    }
    .to_string();
    if critical == Some(true) {
        state.push_str(" (Critical)");
    }
    Some(state)
}

// ---------------- 动态值汇总 ----------------

/// 一次 refresh 计算出的全部动态读数(None = 该来源给不出,由调用方标状态)。
struct Computed {
    state: Option<String>,
    voltage_mv: Option<i64>,
    capacity_mwh: Option<i64>,
    rate_mw: Option<i64>,
    charge_pct: Option<f64>,
    ac_online: Option<bool>,
    run_time_s: Option<f64>,
    charge_time_s: Option<f64>,
    temperature_c: Option<f64>,
}

fn compute(ps: Option<&PsSnapshot>, reg: Option<&RegSnapshot>) -> Computed {
    let voltage_mv = reg
        .and_then(|r| r.voltage_mv)
        .or_else(|| ps.and_then(|p| p.voltage_mv))
        .filter(|v| *v > 0);
    let amperage_ma = reg
        .and_then(|r| r.amperage_ma)
        .or_else(|| ps.and_then(|p| p.current_ma));
    let rate_mw = match (amperage_ma, voltage_mv) {
        (Some(a), Some(v)) => amperage_mv_to_rate_mw(a, v),
        _ => None,
    };
    let mah_cur = reg.and_then(|r| r.remaining_mah).filter(|m| *m > 0);
    let capacity_mwh = match (mah_cur, voltage_mv) {
        (Some(m), Some(v)) => mah_mv_to_mwh(m, v).map(|x| x.round() as i64),
        _ => None,
    };
    // 电量百分比:IOPS 优先;缺席时用注册表容量比(mAh 与百分比归一值同为比值)。
    let reg_pct = reg.and_then(|r| {
        r.remaining_mah
            .zip(r.full_mah)
            .filter(|(c, m)| *c >= 0 && *m > 0)
            .map(|(c, m)| c as f64 / m as f64 * 100.0)
            .or_else(|| {
                r.top_current
                    .zip(r.top_max)
                    .filter(|(c, m)| *c >= 0 && *m > 0)
                    .map(|(c, m)| c as f64 / m as f64 * 100.0)
            })
    });
    let charge_pct = ps
        .and_then(|p| p.pct)
        .or(reg_pct)
        .map(|p| p.clamp(0.0, 100.0));

    let ac_online = ps
        .and_then(|p| p.ac_online)
        .or_else(|| snap_state_is_ac(ps.and_then(|p| p.state.as_deref())))
        .or_else(|| reg.and_then(|r| r.external_connected));

    let state = state_string(
        ac_online,
        ps.and_then(|p| p.charging)
            .or_else(|| reg.and_then(|r| r.is_charging)),
        ps.and_then(|p| p.charged)
            .or_else(|| reg.and_then(|r| r.fully_charged)),
        reg.and_then(|r| r.at_critical),
        charge_pct,
    );

    // 预计续航:IOPS 估计值 > 容量/速率估算(与 Windows 的降级链同构)。
    let discharging = state.as_deref() == Some("Discharging");
    let run_time_s = if discharging {
        ps.and_then(|p| minutes_to_seconds(p.time_to_empty_min.unwrap_or(0)))
            .or_else(|| match (capacity_mwh, rate_mw) {
                (Some(c), Some(r)) if c > 0 && r > 0 => {
                    Some((c as f64 / r as f64 * 3600.0).round())
                }
                _ => None,
            })
    } else {
        None
    };

    // 充满剩余时间:仅充电中,涓流(<100mW)不估算(与 Windows 同阈值)。
    let charging = state.as_deref() == Some("Charging");
    let charge_time_s = if charging {
        ps.and_then(|p| minutes_to_seconds(p.time_to_full_min.unwrap_or(0)))
            .or_else(|| {
                let full_mwh = match (reg.and_then(|r| r.full_mah), voltage_mv) {
                    (Some(m), Some(v)) => mah_mv_to_mwh(m, v).map(|x| x.round() as i64),
                    _ => None,
                };
                match (full_mwh, capacity_mwh, rate_mw) {
                    (Some(f), Some(c), Some(r)) if r < -100 && c < f => {
                        Some(((f - c) as f64 / (-r) as f64 * 3600.0).round())
                    }
                    _ => None,
                }
            })
    } else {
        None
    };

    let temperature_c = reg
        .and_then(|r| r.temperature_raw)
        .and_then(temperature_10k_to_c);

    Computed {
        state,
        voltage_mv,
        capacity_mwh,
        rate_mw,
        charge_pct,
        ac_online,
        run_time_s,
        charge_time_s,
        temperature_c,
    }
}

// ---------------- Provider ----------------

pub struct BatteryProvider;

impl BatteryProvider {
    pub fn new() -> BatteryProvider {
        BatteryProvider
    }
}

impl Default for BatteryProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 动态传感器声明:probe 时拿得到初值就 dynamic_now,拿不到声明 dynamic 等
/// refresh(与 Windows 版同构)。
fn declare(
    b: DeviceBuilder,
    code: &str,
    name: &str,
    unit: &str,
    vtype: ValueType,
    init: Option<Value>,
) -> DeviceBuilder {
    match init {
        Some(v) => b.dynamic_now(code, name, unit, v),
        None => b.dynamic(code, name, unit, vtype),
    }
}

/// 写单个动态读数:拿不到值时按来源逐传感器标状态 —— 来源在但该项未知 =
/// NotSupported,来源整体不可用 = Error。绝不填 0 充数。
fn set(dev: &mut DeviceData, code: &str, vtype: ValueType, value: Option<Value>, source_ok: bool) {
    let reading = match value {
        Some(v) => Reading::now(v),
        None => Reading::failed(
            if source_ok {
                Status::NotSupported
            } else {
                Status::Error
            },
            vtype,
        ),
    };
    dev.set_reading(code, reading);
}

impl Provider for BatteryProvider {
    fn describe(&self) -> &str {
        "mac-battery"
    }

    fn category(&self) -> Category {
        Category::Battery
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let ps = power_source_snapshot();
        let reg = registry_snapshot();
        // 台式机(Mac mini / Mac Studio / Mac Pro):既无电池型电源来源,也无
        // AppleSmartBattery 服务 → 空清单,不算错误(对照 Windows 的台式机行为)。
        let installed = match (&ps, &reg) {
            (Some(_), _) => true,
            (None, Some(r)) => r.battery_installed.unwrap_or(true),
            (None, None) => false,
        };
        if !installed {
            return Ok(Vec::new());
        }

        let mut b = DeviceBuilder::new("battery.0", "Battery 0");
        if let Some(r) = &reg {
            if let Some(n) = &r.device_name {
                b = b.model(n.clone());
            }
            if let Some(m) = &r.manufacturer {
                b = b.vendor(m.clone());
            }
            if let Some(s) = &r.serial {
                b = b.serial(s.clone());
            }
            // 静态识别:mAh 原值与换算 mWh 并存,换算来源与偏差见模块注释。
            let mv_static = r
                .design_voltage_mv
                .filter(|v| *v > 0)
                .or_else(|| r.voltage_mv.filter(|v| *v > 0));
            if let Some(mah) = r.design_mah.filter(|v| *v > 0) {
                b = b.static_f64(
                    "design_capacity_mah",
                    "Design Capacity (mAh)",
                    "mAh",
                    mah as f64,
                );
                if let Some(v) = mv_static.and_then(|v| mah_mv_to_mwh(mah, v)) {
                    b = b.static_f64(
                        "design_capacity_mwh",
                        "Design Capacity",
                        "mWh",
                        (v.round() as u64) as f64,
                    );
                }
            }
            if let Some(mah) = r.full_mah.filter(|v| *v > 0) {
                b = b.static_f64(
                    "full_charge_capacity_mah",
                    "Full Charge Capacity (mAh)",
                    "mAh",
                    mah as f64,
                );
                if let Some(v) = mv_static.and_then(|v| mah_mv_to_mwh(mah, v)) {
                    b = b.static_f64(
                        "full_charge_capacity_mwh",
                        "Full Charge Capacity",
                        "mWh",
                        (v.round() as u64) as f64,
                    );
                }
                // 健康度与 Windows 同公式:满充/设计。用 mAh 比值(单位无关,
                // 比换算 mWh 更稳)。macOS 界面的"最大容量"是另一口径
                // (NominalChargeCapacity/DesignCapacity),数值可能略高,不混用。
                if let Some(design) = r.design_mah.filter(|v| *v > 0) {
                    let health = mah as f64 / design as f64 * 100.0;
                    if (0.0..=150.0).contains(&health) {
                        b = b.static_f64(
                            "health_pct",
                            "Battery Health",
                            "%",
                            (health * 10.0).round() / 10.0,
                        );
                    }
                }
            }
            if let Some(c) = r.cycle_count.filter(|v| *v > 0) {
                b = b.static_f64("cycle_count", "Cycle Count", "", c as f64);
                // 有循环计数即为可充电池(SBS 循环计数只对可充电池有意义)。
                b = b.static_str("rechargeable", "Rechargeable", "Yes");
            }
            if let Some(date) = r.manufacture_date_raw.and_then(decode_manufacture_date) {
                b = b.static_str("manufacture_date", "Manufacture Date", date);
            }
        }

        let c = compute(ps.as_ref(), reg.as_ref());
        b = declare(
            b,
            "state",
            "State",
            "",
            ValueType::Str,
            c.state.clone().map(Value::Str),
        );
        b = declare(
            b,
            "voltage_mv",
            "Voltage",
            "mV",
            ValueType::U64,
            c.voltage_mv.map(|v| Value::U64(v as u64)),
        );
        b = declare(
            b,
            "capacity_mwh",
            "Remaining Capacity",
            "mWh",
            ValueType::U64,
            c.capacity_mwh.map(|v| Value::U64(v as u64)),
        );
        b = declare(
            b,
            "rate_mw",
            "Rate",
            "mW",
            ValueType::I64,
            c.rate_mw.map(Value::I64),
        );
        b = declare(
            b,
            "charge_pct",
            "Charge",
            "%",
            ValueType::F64,
            c.charge_pct.map(Value::F64),
        );
        b = declare(
            b,
            "ac_online",
            "AC Online",
            "",
            ValueType::Bool,
            c.ac_online.map(Value::Bool),
        );
        b = declare(
            b,
            "run_time_s",
            "Est. Run Time",
            "s",
            ValueType::F64,
            c.run_time_s.map(Value::F64),
        );
        b = declare(
            b,
            "charge_time_s",
            "Est. Time To Full",
            "s",
            ValueType::F64,
            c.charge_time_s.map(Value::F64),
        );
        // 温度:注册表有 Temperature 键才声明(与 Windows"驱动支持才声明"同构)。
        // Apple Silicon 机型普遍没有该键,如实缺省。
        if reg.as_ref().and_then(|r| r.temperature_raw).is_some() {
            b = declare(
                b,
                "temperature_c",
                "Temperature",
                "°C",
                ValueType::F64,
                c.temperature_c.map(Value::F64),
            );
        }
        Ok(vec![b.build()])
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(2000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let ps = power_source_snapshot();
        let reg = registry_snapshot();
        let ps_ok = ps.is_some();
        let reg_ok = reg.is_some();
        let c = compute(ps.as_ref(), reg.as_ref());
        for dev in devices.iter_mut() {
            let any_ok = ps_ok || reg_ok;
            set(
                dev,
                "state",
                ValueType::Str,
                c.state.clone().map(Value::Str),
                any_ok,
            );
            set(
                dev,
                "voltage_mv",
                ValueType::U64,
                c.voltage_mv.map(|v| Value::U64(v as u64)),
                any_ok,
            );
            // 容量换算依赖注册表的 mAh 原值,来源只算注册表。
            set(
                dev,
                "capacity_mwh",
                ValueType::U64,
                c.capacity_mwh.map(|v| Value::U64(v as u64)),
                reg_ok,
            );
            set(
                dev,
                "rate_mw",
                ValueType::I64,
                c.rate_mw.map(Value::I64),
                any_ok,
            );
            set(
                dev,
                "charge_pct",
                ValueType::F64,
                c.charge_pct.map(Value::F64),
                any_ok,
            );
            set(
                dev,
                "ac_online",
                ValueType::Bool,
                c.ac_online.map(Value::Bool),
                any_ok,
            );
            set(
                dev,
                "run_time_s",
                ValueType::F64,
                c.run_time_s.map(Value::F64),
                any_ok,
            );
            set(
                dev,
                "charge_time_s",
                ValueType::F64,
                c.charge_time_s.map(Value::F64),
                any_ok,
            );
            // 仅声明过的传感器会被写入,没声明温度的机型不受影响。
            set(
                dev,
                "temperature_c",
                ValueType::F64,
                c.temperature_c.map(Value::F64),
                reg_ok,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_conversion() {
        // 3031 → 303.1K = 29.95°C(与 Windows 的 0.1K 口径一致)
        let t = temperature_10k_to_c(3031).unwrap();
        assert!((t - 29.95).abs() < 0.06);
        assert_eq!(temperature_10k_to_c(0), None);
        assert_eq!(temperature_10k_to_c(50_000), None); // 2268°C,显然无效
    }

    #[test]
    fn capacity_conversion() {
        // 实测机型:DesignCapacity 6075mAh × 12603mV ≈ 76.6Wh 量级
        let wh = mah_mv_to_mwh(6075, 12603).unwrap();
        assert!((wh - 76_563.0).abs() < 2.0);
        // 1Ah × 1V = 1Wh = 1000mWh(换算系数的锚点)
        assert_eq!(mah_mv_to_mwh(1000, 1000), Some(1000.0));
        assert_eq!(mah_mv_to_mwh(1000, 0), None);
        assert_eq!(mah_mv_to_mwh(-1, 12_000), None);
    }

    #[test]
    fn rate_conversion_and_sign() {
        // Apple 约定负=放电:12603mV × -1500mA ≈ 18.9W 放电 → 正(Windows 口径)
        assert_eq!(amperage_mv_to_rate_mw(-1500, 12_603), Some(18_904));
        // 充电(正电流)→ 负 mW
        assert!(amperage_mv_to_rate_mw(800, 12_000).unwrap() < 0);
        // mV·mA 是 µW 量级,必须除 1000:2000mA × 1000mV = 2000mW(不是 2e6)
        assert_eq!(amperage_mv_to_rate_mw(-2000, 1000), Some(2000));
    }

    #[test]
    fn minutes_guard() {
        assert_eq!(minutes_to_seconds(60), Some(3600.0));
        assert_eq!(minutes_to_seconds(0), None); // 计算中/未知
        assert_eq!(minutes_to_seconds(65535), None); // 0xFFFF 哨兵
        assert_eq!(minutes_to_seconds(24 * 60 + 1), None); // 超过一天,不可信
    }

    #[test]
    fn manufacture_date() {
        // SBS 0x1B:2026-10-04 → day=4, month=10, year=46(自 1980 起)
        let raw = 4 | (10 << 5) | (46 << 9);
        assert_eq!(decode_manufacture_date(raw).as_deref(), Some("2026-10-04"));
        assert_eq!(decode_manufacture_date(13 << 5), None); // 月份 13
        assert_eq!(decode_manufacture_date(0x1_0000), None); // 越界
    }

    #[test]
    fn state_strings_match_windows() {
        assert_eq!(
            state_string(Some(true), Some(true), Some(false), None, Some(80.0)).as_deref(),
            Some("Charging")
        );
        assert_eq!(
            state_string(Some(false), Some(false), Some(false), None, Some(50.0)).as_deref(),
            Some("Discharging")
        );
        assert_eq!(
            state_string(Some(false), Some(false), Some(false), Some(true), Some(5.0)).as_deref(),
            Some("Discharging (Critical)")
        );
        assert_eq!(
            state_string(Some(true), Some(false), Some(true), None, Some(100.0)).as_deref(),
            Some("AC Power (Fully Charged)")
        );
        // 与 Windows 一致:≥99% 也按满充标注
        assert_eq!(
            state_string(Some(true), Some(false), Some(false), None, Some(99.0)).as_deref(),
            Some("AC Power (Fully Charged)")
        );
        assert_eq!(
            state_string(Some(true), Some(false), Some(false), None, Some(80.0)).as_deref(),
            Some("AC Power")
        );
        assert_eq!(
            state_string(None, Some(false), None, None, None).as_deref(),
            Some("Idle")
        );
        // 供电与充电状态都读不到 → 不编造状态
        assert_eq!(state_string(None, None, None, None, None), None);
    }
}
