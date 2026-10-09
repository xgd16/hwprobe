//! 电池 provider:电池类设备 IOCTL(batclass)+ GetSystemPowerStatus。
//!
//! 静态:BATTERY_INFORMATION(化学类型/设计容量/满充容量/告警阈值/循环次数)、
//!       设备名/厂商/序列号/唯一 ID/生产日期。
//! 动态:状态(含 Critical)、电压、剩余容量、充放速率、电量百分比(单电池
//!       剩余/满充,失败回退系统级)、温度(0.1K 单位)、预计续航 / 充满时间。
//! 台式机没有电池设备时返回空清单(不算错误)。
//!
//! 未知值防护:batclass 以 0 表示电压未知、0xFFFFFFFF 表示容量/速率/时间未知,
//! 统一映射为 NOT_SUPPORTED,不输出误导值。

#![cfg(target_os = "windows")]

use crate::model::{
    Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType,
};
use crate::traits::Provider;
use std::time::Duration;
use windows::core::GUID;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::{
    GetSystemPowerStatus, SYSTEM_POWER_STATUS,
};

// GUID_DEVCLASS_BATTERY {72631E54-78A4-11D0-BCF7-00AA00B7B32A}
const GUID_DEVCLASS_BATTERY: GUID = GUID::from_u128(0x72631E54_78A4_11D0_BCF7_00AA00B7B32A);

// batclass.h IOCTL 定义:CTL_CODE(DeviceType, Function, Method, Access)
const FILE_DEVICE_BATTERY: u32 = 0x0000_0029;
const FILE_READ_ACCESS: u32 = 0x0001;
const FILE_WRITE_ACCESS: u32 = 0x0002;
const METHOD_BUFFERED: u32 = 0;
const fn ctl_code(dev: u32, func: u32, method: u32, access: u32) -> u32 {
    (dev << 16) | (access << 14) | (func << 2) | method
}
// 功能号与访问位取自 batclass.h / poclass.h:QUERY_TAG=0x10、QUERY_INFORMATION=0x11、
// QUERY_STATUS=0x13(不是 0/1/3),三条都只要求 FILE_READ_ACCESS。
// 访问位写错(如 QUERY_TAG 写成 READ|WRITE)时 battc 不报错,而是回成功但
// 0 字节/tag=0,后续查询随之全部失败——表现为"设备存在但打不开"。
const IOCTL_BATTERY_QUERY_TAG: u32 =
    ctl_code(FILE_DEVICE_BATTERY, 0x10, METHOD_BUFFERED, FILE_READ_ACCESS);
const IOCTL_BATTERY_QUERY_INFORMATION: u32 =
    ctl_code(FILE_DEVICE_BATTERY, 0x11, METHOD_BUFFERED, FILE_READ_ACCESS);
const IOCTL_BATTERY_QUERY_STATUS: u32 =
    ctl_code(FILE_DEVICE_BATTERY, 0x13, METHOD_BUFFERED, FILE_READ_ACCESS);

// BATTERY_QUERY_INFORMATION_LEVEL
const BATTERY_INFO_INFORMATION: u32 = 0;
const BATTERY_INFO_TEMPERATURE: u32 = 2;
const BATTERY_INFO_ESTIMATED_TIME: u32 = 3;
const BATTERY_INFO_MANUFACTURE_DATE: u32 = 5;
const BATTERY_INFO_DEVICE_NAME: u32 = 4;
const BATTERY_INFO_MANUFACTURE_NAME: u32 = 6;
const BATTERY_INFO_UNIQUE_ID: u32 = 7;
const BATTERY_INFO_SERIAL_NUMBER: u32 = 8;

// BATTERY_STATUS.PowerState 位。位值以 SDK 头文件 poclass.h 为准
// (POWER_ON_LINE=1、DISCHARGING=2、CHARGING=4、CRITICAL=8);
// MSDN 的 BATTERY_STATUS 文档表格把前三个值写反了,勿照抄文档。
const BATTERY_POWER_ONLINE: u32 = 0x0000_0001;
const BATTERY_DISCHARGING: u32 = 0x0000_0002;
const BATTERY_CHARGING: u32 = 0x0000_0004;
const BATTERY_CRITICAL: u32 = 0x0000_0008;

// batclass 未知值约定
const BATTERY_UNKNOWN_CAPACITY: u32 = 0xFFFF_FFFF;
const BATTERY_UNKNOWN_RATE: u32 = 0xFFFF_FFFF;
const BATTERY_UNKNOWN_TIME: u32 = 0xFFFF_FFFF;

// BATTERY_INFORMATION.Capabilities 位
const BATTERY_RELATIVE_TO_FULL_CHARGE: u32 = 0x0000_0004;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BatteryQueryInfoInput {
    tag: u32,
    level: u32,
    at_rate: i32,
}

/// BATTERY_WAIT_STATUS(poclass.h):5 个 ULONG,共 20 字节。
/// 少写字段会让输入缓冲区偏小,battc 直接回 ERROR_INVALID_USER_BUFFER(1784)。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BatteryWaitStatus {
    tag: u32,
    timeout: u32,
    power_state: u32,
    low_capacity: u32,
    high_capacity: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BatteryStatusOut {
    power_state: u32,
    capacity: u32, // mWh(或相对单位,见 BATTERY_RELATIVE_TO_FULL_CHARGE)
    voltage: u32,  // mV,0 = 未知
    rate: i32,     // mW,正值放电、负值充电,0xFFFFFFFF = 未知
}

/// BATTERY_INFORMATION 解码结果(batclass.h 布局:Capabilities@0,
/// Technology@4, Chemistry@8, DesignedCapacity@12, FullChargedCapacity@16,
/// DefaultAlert1@20, DefaultAlert2@24, CriticalBias@28, CycleCount@32)。
#[derive(Debug, Default, Clone, Copy)]
struct BatteryInfoRaw {
    capabilities: u32,
    technology: u8,
    chemistry: [u8; 4],
    designed_capacity: u32,
    full_charged_capacity: u32,
    default_alert1: u32,
    default_alert2: u32,
    critical_bias: u32,
    cycle_count: u32,
}

impl BatteryInfoRaw {
    fn parse(buf: &[u8]) -> Option<BatteryInfoRaw> {
        if buf.len() < 36 {
            return None;
        }
        let u32_at = |off: usize| {
            u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
        };
        Some(BatteryInfoRaw {
            capabilities: u32_at(0),
            technology: buf[4],
            chemistry: [buf[8], buf[9], buf[10], buf[11]],
            designed_capacity: u32_at(12),
            full_charged_capacity: u32_at(16),
            default_alert1: u32_at(20),
            default_alert2: u32_at(24),
            critical_bias: u32_at(28),
            cycle_count: u32_at(32),
        })
    }

    /// 容量是否为相对满充的百分比单位而非 mWh。
    fn relative_capacity(&self) -> bool {
        self.capabilities & BATTERY_RELATIVE_TO_FULL_CHARGE != 0
    }
}

pub struct BatteryProvider;

impl BatteryProvider {
    pub fn new() -> BatteryProvider {
        BatteryProvider
    }

    /// 枚举电池设备路径(\\.\BATTERY0 ...)。
    fn battery_paths() -> Vec<String> {
        unsafe {
            let mut out = Vec::new();
            let hdev = SetupDiGetClassDevsW(
                Some(&GUID_DEVCLASS_BATTERY),
                None,
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
            .unwrap_or_default();
            if hdev.is_invalid() {
                return out;
            }
            let mut i = 0u32;
            loop {
                let mut ifdata = SP_DEVICE_INTERFACE_DATA {
                    cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInterfaces(hdev, None, &GUID_DEVCLASS_BATTERY, i, &mut ifdata)
                    .is_err()
                {
                    break;
                }
                // 第一次调用取所需长度
                let mut needed = 0u32;
                let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &ifdata, None, 0, Some(&mut needed), None);
                if needed == 0 {
                    i += 1;
                    continue;
                }
                // 手动布局:cbSize(u32, x64 上要求 8)+ DevicePath(WCHAR 变长)
                let mut buf = vec![0u8; needed as usize];
                buf[0..4].copy_from_slice(&8u32.to_le_bytes());
                let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
                if SetupDiGetDeviceInterfaceDetailW(
                    hdev,
                    &ifdata,
                    Some(detail),
                    needed,
                    None,
                    None,
                )
                .is_ok()
                {
                    // DevicePath 位于 cbSize 之后(offset 4)
                    let mut path = String::new();
                    let mut p = (detail as *const u8).add(4) as *const u16;
                    for _ in 0..1024 {
                        let c = *p;
                        if c == 0 {
                            break;
                        }
                        path.push(char::from_u32(c as u32).unwrap_or('?'));
                        p = p.add(1);
                    }
                    if path.starts_with("\\\\?\\") {
                        out.push(path);
                    }
                }
                i += 1;
            }
            let _ = SetupDiDestroyDeviceInfoList(hdev);
            out
        }
    }

    fn open(path: &str) -> Result<windows::Win32::Foundation::HANDLE, String> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let h = CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .map_err(|e| format!("CreateFile({path}) 失败: {e}"))?;
            Ok(h)
        }
    }

    fn query_tag(handle: windows::Win32::Foundation::HANDLE) -> Result<u32, String> {
        unsafe {
            // IOCTL_BATTERY_QUERY_TAG 的输入是单个 ULONG 超时(毫秒),0 = 不等;
            // 传更大的结构体会被拒(1784)。
            let mut timeout: u32 = 0;
            let mut tag: u32 = 0;
            let mut ret = 0u32;
            let ok = DeviceIoControl(
                handle,
                IOCTL_BATTERY_QUERY_TAG,
                Some(&mut timeout as *mut _ as *mut _),
                4,
                Some(&mut tag as *mut _ as *mut _),
                4,
                Some(&mut ret),
                None,
            )
            .map_err(|e| format!("IOCTL_BATTERY_QUERY_TAG 失败: {e}"))?;
            let _ = ok;
            Ok(tag)
        }
    }

    fn query_info_raw(
        handle: windows::Win32::Foundation::HANDLE,
        tag: u32,
        level: u32,
        at_rate: i32,
        buf_len: usize,
    ) -> Option<Vec<u8>> {
        unsafe {
            let mut input = BatteryQueryInfoInput { tag, level, at_rate };
            let mut buf = vec![0u8; buf_len];
            let mut ret = 0u32;
            DeviceIoControl(
                handle,
                IOCTL_BATTERY_QUERY_INFORMATION,
                Some(&mut input as *mut _ as *mut _),
                std::mem::size_of::<BatteryQueryInfoInput>() as u32,
                Some(buf.as_mut_ptr() as *mut _),
                buf.len() as u32,
                Some(&mut ret),
                None,
            )
            .ok()?;
            Some(buf)
        }
    }

    fn query_info_string(
        handle: windows::Win32::Foundation::HANDLE,
        tag: u32,
        level: u32,
    ) -> Option<String> {
        let buf = Self::query_info_raw(handle, tag, level, 0, 256)?;
        decode_info_string(&buf)
    }

    fn query_info_u32(
        handle: windows::Win32::Foundation::HANDLE,
        tag: u32,
        level: u32,
    ) -> Option<u32> {
        let buf = Self::query_info_raw(handle, tag, level, 0, 8)?;
        if buf.len() < 4 {
            return None;
        }
        Some(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
    }

    fn query_status(
        handle: windows::Win32::Foundation::HANDLE,
        tag: u32,
    ) -> Result<BatteryStatusOut, String> {
        unsafe {
            let mut input = BatteryWaitStatus {
                tag,
                ..Default::default()
            };
            let mut out = BatteryStatusOut::default();
            let mut ret = 0u32;
            DeviceIoControl(
                handle,
                IOCTL_BATTERY_QUERY_STATUS,
                Some(&mut input as *mut _ as *mut _),
                std::mem::size_of::<BatteryWaitStatus>() as u32,
                Some(&mut out as *mut _ as *mut _),
                std::mem::size_of::<BatteryStatusOut>() as u32,
                Some(&mut ret),
                None,
            )
            .map_err(|e| format!("IOCTL_BATTERY_QUERY_STATUS 失败: {e}"))?;
            Ok(out)
        }
    }
}

// ---------------- 纯解码逻辑(可单测) ----------------

/// DeviceName / ManufactureName / UniqueID / SerialNumber 这几级返回的是以 NUL
/// 结尾的 UTF-16LE 宽字符串;按单字节解码会把 "BIF0_9" 截成 "B"。
/// 少数第三方 miniclass 驱动可能返回 ANSI,出现替换字符时退回单字节解析。
fn decode_info_string(buf: &[u8]) -> Option<String> {
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    let wide = String::from_utf16_lossy(&units[..end]);

    let s = if wide.contains('\u{FFFD}') {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).to_string()
    } else {
        wide
    };
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 化学类型缩写 → 全称;无法识别但可打印的按原样透出,不可解码给空。
fn chemistry_name(chem: &[u8]) -> String {
    let s = String::from_utf8_lossy(chem).trim_end_matches('\0').trim().to_string();
    if s.contains('\u{FFFD}') {
        return String::new();
    }
    match s.as_str() {
        "PbA" | "PbAc" => "Lead-Acid",
        "LION" | "Li-I" => "Li-Ion",
        "Li-P" => "Li-Polymer",
        "NiCd" => "Ni-Cd",
        "NiMH" => "Ni-MH",
        "NiZn" => "Ni-Zn",
        "Alka" => "Alkaline",
        "RAM" => "Rechargeable Alkaline",
        "Zinc-Air" => "Zinc-Air",
        "" => "",
        other => other,
    }
    .to_string()
}

/// IOCTL BatteryManufactureDate:FAT 日期打包(day:5, month:4, year:7 自 1980 起)。
fn decode_manufacture_date(raw: u32) -> Option<String> {
    let day = raw & 0x1F;
    let month = (raw >> 5) & 0x0F;
    let year = 1980 + ((raw >> 9) & 0x7F);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1980..=2107).contains(&year) {
        return None;
    }
    Some(format!("{year}-{month:02}-{day:02}"))
}

/// IOCTL BatteryTemperature:单位 0.1K → °C。越界视为不支持。
fn temperature_10k_to_c(raw: u32) -> Option<f64> {
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

/// PowerState 位 → 状态字符串(含 Critical 后缀;满充且在交流电时标注)。
fn state_string(power_state: u32, charge_pct: Option<f64>) -> String {
    let mut state = if power_state & BATTERY_CHARGING != 0 {
        "Charging"
    } else if power_state & BATTERY_DISCHARGING != 0 {
        "Discharging"
    } else if power_state & BATTERY_POWER_ONLINE != 0 {
        match charge_pct {
            Some(p) if p >= 99.0 => "AC Power (Fully Charged)",
            _ => "AC Power",
        }
    } else {
        "Idle"
    }
    .to_string();
    if power_state & BATTERY_CRITICAL != 0 {
        state.push_str(" (Critical)");
    }
    state
}

/// 单电池电量百分比:剩余/满充(单位无关,相对容量标记下同样成立)。
fn pct_of_full(capacity: u32, full: u32) -> Option<f64> {
    if capacity == 0 || capacity == BATTERY_UNKNOWN_CAPACITY || full == 0 {
        return None;
    }
    Some(((capacity as f64 / full as f64) * 100.0).clamp(0.0, 100.0))
}

/// BATTERY_STATUS.Rate 是否为有效速率(0xFFFFFFFF = 未知)。
fn rate_valid(rate: i32) -> bool {
    (rate as u32) != BATTERY_UNKNOWN_RATE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_info_offsets() {
        // CycleCount 在偏移 32(CriticalBias@28 不可混用)
        let mut buf = vec![0u8; 36];
        buf[4] = 1; // Technology = rechargeable
        buf[8..12].copy_from_slice(b"LION");
        buf[12..16].copy_from_slice(&50_000u32.to_le_bytes());
        buf[16..20].copy_from_slice(&46_000u32.to_le_bytes());
        buf[20..24].copy_from_slice(&4_600u32.to_le_bytes());
        buf[24..28].copy_from_slice(&1_800u32.to_le_bytes());
        buf[28..32].copy_from_slice(&123u32.to_le_bytes());
        buf[32..36].copy_from_slice(&321u32.to_le_bytes());
        let info = BatteryInfoRaw::parse(&buf).unwrap();
        assert_eq!(info.designed_capacity, 50_000);
        assert_eq!(info.full_charged_capacity, 46_000);
        assert_eq!(info.default_alert1, 4_600);
        assert_eq!(info.default_alert2, 1_800);
        assert_eq!(info.critical_bias, 123);
        assert_eq!(info.cycle_count, 321);
        assert_eq!(info.technology, 1);
        assert_eq!(chemistry_name(&info.chemistry), "Li-Ion");
        assert!(BatteryInfoRaw::parse(&buf[..35]).is_none());
    }

    #[test]
    fn chemistry_mapping() {
        assert_eq!(chemistry_name(b"NiMH\0"), "Ni-MH");
        assert_eq!(chemistry_name(b"LION\0"), "Li-Ion");
        // 未收录但可打印的透传,乱码给空
        assert_eq!(chemistry_name(b"XYZA\0"), "XYZA");
        assert_eq!(chemistry_name(b"\xff\xfe\xfd\x00"), "");
    }

    #[test]
    fn manufacture_date() {
        // 2026-10-04 → year=46, month=10, day=4
        let raw = (4) | (10 << 5) | (46 << 9);
        assert_eq!(decode_manufacture_date(raw).as_deref(), Some("2026-10-04"));
        assert_eq!(decode_manufacture_date(0), None);
        assert_eq!(decode_manufacture_date(13 << 5), None); // 月份 13
    }

    #[test]
    fn temperature_conversion() {
        let t = temperature_10k_to_c(2982).unwrap(); // 298.2K = 25.05°C
        assert!((t - 25.05).abs() < 0.06);
        assert_eq!(temperature_10k_to_c(0), None);
        assert_eq!(temperature_10k_to_c(50_000), None); // 2268°C,显然无效
    }

    #[test]
    fn state_and_pct() {
        // 用 poclass.h 的原始位值钉死:常量若再写错(如文档反值)立刻在这里爆。
        assert_eq!(BATTERY_POWER_ONLINE, 0x1);
        assert_eq!(BATTERY_DISCHARGING, 0x2);
        assert_eq!(BATTERY_CHARGING, 0x4);
        assert_eq!(BATTERY_CRITICAL, 0x8);
        assert_eq!(state_string(0x2, None), "Discharging"); // 拔电使用电池
        assert_eq!(
            state_string(0x2 | 0x8, None),
            "Discharging (Critical)"
        );
        assert_eq!(state_string(0x5, Some(80.0)), "Charging"); // 插电充电中(0x1|0x4)
        assert_eq!(state_string(0x1, Some(100.0)), "AC Power (Fully Charged)");
        assert_eq!(state_string(0x1, Some(80.0)), "AC Power");
        assert_eq!(state_string(0x0, None), "Idle");
        assert_eq!(pct_of_full(23_000, 46_000), Some(50.0));
        assert_eq!(pct_of_full(0, 46_000), None);
        assert_eq!(pct_of_full(BATTERY_UNKNOWN_CAPACITY, 46_000), None);
        assert_eq!(pct_of_full(23_000, 0), None);
    }

    #[test]
    fn info_string_decoding() {
        // Claw A1M 实测:batclass 返回 UTF-16LE,按单字节解码会被截成 "B"。
        let wide = |s: &str| -> Vec<u8> {
            s.encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(|c| c.to_le_bytes())
                .collect()
        };
        assert_eq!(decode_info_string(&wide("BIF0_9")).as_deref(), Some("BIF0_9"));
        assert_eq!(decode_info_string(&wide("MSI")).as_deref(), Some("MSI"));
        assert_eq!(decode_info_string(&wide("  MSIBIF0_9 ")).as_deref(), Some("MSIBIF0_9"));
        // 空串/只有 NUL → None,不产出空设备名
        assert_eq!(decode_info_string(&wide("")), None);
        assert_eq!(decode_info_string(&[0, 0, 0, 0]), None);
        // 奇数长度缓冲不越界
        assert_eq!(decode_info_string(&[b'B', 0, b'I']).as_deref(), Some("B"));
    }
}

// ---------------- Provider 实现 ----------------

impl Provider for BatteryProvider {
    fn describe(&self) -> &str {
        "win-battery"
    }

    fn category(&self) -> Category {
        Category::Battery
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let paths = Self::battery_paths();
        if paths.is_empty() {
            return Err("无电池设备(台式机属正常情况)".into());
        }
        let mut devices = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let handle = match Self::open(path) {
                Ok(h) => h,
                Err(e) => {
                    crate::logbuf::warn(format!("battery: {e}"));
                    continue;
                }
            };
            unsafe {
                let tag = match Self::query_tag(handle) {
                    Ok(t) if t != 0 => t,
                    Ok(_) => {
                        // 设备在但报 tag=0:多半是 QUERY_TAG 的调用方式不对
                        // (访问位/输入长度),没有 tag 后续查询必然全失败。
                        crate::logbuf::warn(format!(
                            "battery: {path} 报告 tag=0(无有效电池标签),跳过"
                        ));
                        windows::Win32::Foundation::CloseHandle(handle).ok();
                        continue;
                    }
                    Err(e) => {
                        crate::logbuf::warn(format!("battery: {e}"));
                        windows::Win32::Foundation::CloseHandle(handle).ok();
                        continue;
                    }
                };
                let mut b = DeviceBuilder::new(format!("battery.{i}"), format!("Battery {i}"));
                if let Some(n) = Self::query_info_string(handle, tag, BATTERY_INFO_DEVICE_NAME) {
                    if !n.is_empty() {
                        b = b.model(n);
                    }
                }
                if let Some(m) = Self::query_info_string(handle, tag, BATTERY_INFO_MANUFACTURE_NAME) {
                    if !m.is_empty() {
                        b = b.vendor(m);
                    }
                }
                if let Some(s) = Self::query_info_string(handle, tag, BATTERY_INFO_SERIAL_NUMBER) {
                    if !s.is_empty() {
                        b = b.serial(s);
                    }
                }
                if let Some(u) = Self::query_info_string(handle, tag, BATTERY_INFO_UNIQUE_ID) {
                    if !u.is_empty() {
                        b = b.static_str("unique_id", "Unique ID", u);
                    }
                }
                if let Some(d) = Self::query_info_u32(handle, tag, BATTERY_INFO_MANUFACTURE_DATE) {
                    if let Some(date) = decode_manufacture_date(d) {
                        b = b.static_str("manufacture_date", "Manufacture Date", date);
                    }
                }

                // BATTERY_INFORMATION:容量体系 + 告警阈值 + 循环次数
                let mut full_charge = 0u32;
                let mut relative = false;
                if let Some(buf) = Self::query_info_raw(handle, tag, BATTERY_INFO_INFORMATION, 0, 64) {
                    if let Some(info) = BatteryInfoRaw::parse(&buf) {
                        relative = info.relative_capacity();
                        let chem = chemistry_name(&info.chemistry);
                        if !chem.is_empty() {
                            b = b.static_str("chemistry", "Chemistry", chem);
                        }
                        if info.technology == 1 {
                            b = b.static_str("rechargeable", "Rechargeable", "Yes");
                        } else if info.technology == 0 {
                            b = b.static_str("rechargeable", "Rechargeable", "No");
                        }
                        // BATTERY_RELATIVE_TO_FULL_CHARGE 时容量为相对满充的百分比单位
                        let cap_unit = if relative { "rel" } else { "mWh" };
                        if info.designed_capacity > 0 && info.designed_capacity != BATTERY_UNKNOWN_CAPACITY {
                            b = b.static_f64(
                                "design_capacity_mwh",
                                "Design Capacity",
                                cap_unit,
                                info.designed_capacity as f64,
                            );
                        }
                        if info.full_charged_capacity > 0 && info.full_charged_capacity != BATTERY_UNKNOWN_CAPACITY {
                            full_charge = info.full_charged_capacity;
                            b = b.static_f64(
                                "full_charge_capacity_mwh",
                                "Full Charge Capacity",
                                cap_unit,
                                info.full_charged_capacity as f64,
                            );
                        }
                        if full_charge > 0
                            && info.designed_capacity > 0
                            && info.designed_capacity != BATTERY_UNKNOWN_CAPACITY
                        {
                            let health = info.full_charged_capacity as f64 / info.designed_capacity as f64 * 100.0;
                            if (0.0..=150.0).contains(&health) {
                                b = b.static_f64("health_pct", "Battery Health", "%", (health * 10.0).round() / 10.0);
                            }
                        }
                        if info.default_alert1 > 0 && info.default_alert1 != BATTERY_UNKNOWN_CAPACITY {
                            b = b.static_f64("low_alert_mwh", "Low Alert Level", cap_unit, info.default_alert1 as f64);
                        }
                        if info.default_alert2 > 0 && info.default_alert2 != BATTERY_UNKNOWN_CAPACITY {
                            b = b.static_f64("critical_alert_mwh", "Critical Alert Level", cap_unit, info.default_alert2 as f64);
                        }
                        if info.critical_bias > 0 && info.critical_bias != BATTERY_UNKNOWN_CAPACITY {
                            b = b.static_f64("critical_bias_mw", "Critical Bias", "mW", info.critical_bias as f64);
                        }
                        if info.cycle_count > 0 && info.cycle_count != BATTERY_UNKNOWN_CAPACITY {
                            b = b.static_f64("cycle_count", "Cycle Count", "", info.cycle_count as f64);
                        }
                    }
                }

                // 温度:驱动支持才声明(单位 0.1K)
                if let Some(t) = Self::query_info_u32(handle, tag, BATTERY_INFO_TEMPERATURE)
                    .and_then(temperature_10k_to_c)
                {
                    b = b.dynamic_now("temperature_c", "Temperature", "°C", Value::F64(t));
                }

                // 首次状态读取,给动态传感器真实初值
                let mut status = None;
                if let Ok(st) = Self::query_status(handle, tag) {
                    status = Some(st);
                }

                // 电量百分比:优先单电池 剩余/满充,拿不到回退系统级
                let dev_pct = status
                    .as_ref()
                    .and_then(|st| pct_of_full(st.capacity, full_charge));
                let mut sps = SYSTEM_POWER_STATUS::default();
                let sps_ok = GetSystemPowerStatus(&mut sps).is_ok();
                let pct = dev_pct.or_else(|| {
                    sps_ok
                        .then_some(sps.BatteryLifePercent)
                        .filter(|p| *p <= 100)
                        .map(|p| p as f64)
                });

                match &status {
                    Some(st) => {
                        b = b.dynamic_now("state", "State", "", Value::Str(state_string(st.power_state, pct).into()));
                        if st.voltage > 0 {
                            b = b.dynamic_now("voltage_mv", "Voltage", "mV", Value::U64(st.voltage as u64));
                        } else {
                            b = b.dynamic("voltage_mv", "Voltage", "mV", ValueType::U64);
                        }
                        if st.capacity != BATTERY_UNKNOWN_CAPACITY {
                            b = b.dynamic_now("capacity_mwh", "Remaining Capacity", cap_unit_of(relative), Value::U64(st.capacity as u64));
                        } else {
                            b = b.dynamic("capacity_mwh", "Remaining Capacity", cap_unit_of(relative), ValueType::U64);
                        }
                        if rate_valid(st.rate) {
                            b = b.dynamic_now("rate_mw", "Rate", "mW", Value::I64(st.rate as i64));
                        } else {
                            b = b.dynamic("rate_mw", "Rate", "mW", ValueType::I64);
                        }
                    }
                    None => {
                        b = b.dynamic("state", "State", "", ValueType::Str)
                            .dynamic("voltage_mv", "Voltage", "mV", ValueType::U64)
                            .dynamic("capacity_mwh", "Remaining Capacity", cap_unit_of(relative), ValueType::U64)
                            .dynamic("rate_mw", "Rate", "mW", ValueType::I64);
                    }
                }

                match pct {
                    Some(p) => b = b.dynamic_now("charge_pct", "Charge", "%", Value::F64(p)),
                    None => b = b.dynamic("charge_pct", "Charge", "%", ValueType::F64),
                }
                b = b.dynamic("ac_online", "AC Online", "", ValueType::Bool);
                b = b.dynamic("run_time_s", "Est. Run Time", "s", ValueType::F64);
                b = b.dynamic("charge_time_s", "Est. Time To Full", "s", ValueType::F64);
                devices.push(b.build());
                windows::Win32::Foundation::CloseHandle(handle).ok();
            }
        }
        if devices.is_empty() {
            return Err("电池设备存在但均不可用(打不开或无有效电池标签)".into());
        }
        Ok(devices)
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(2000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let paths = Self::battery_paths();
        for (i, dev) in devices.iter_mut().enumerate() {
            let Some(path) = paths.get(i) else {
                continue;
            };
            let handle = match Self::open(path) {
                Ok(h) => h,
                Err(_) => {
                    for s in dev.sensors.iter_mut() {
                        if s.desc.kind == crate::model::SensorKind::Dynamic {
                            s.current = Reading::failed(Status::Error, s.desc.vtype);
                        }
                    }
                    continue;
                }
            };
            unsafe {
                let mut status = None;
                let tag = Self::query_tag(handle).ok().filter(|t| *t != 0);
                if let Some(tag) = tag {
                    if let Ok(st) = Self::query_status(handle, tag) {
                        status = Some(st);
                    }
                    // 温度(声明了才读)
                    if dev.sensors.iter().any(|s| s.desc.code == "temperature_c") {
                        match Self::query_info_u32(handle, tag, BATTERY_INFO_TEMPERATURE)
                            .and_then(temperature_10k_to_c)
                        {
                            Some(t) => dev.set_reading("temperature_c", Reading::now(Value::F64(t))),
                            None => dev.set_reading(
                                "temperature_c",
                                Reading::failed(Status::NotSupported, ValueType::F64),
                            ),
                        }
                    }
                }
                let full_charge = dev
                    .sensors
                    .iter()
                    .find(|s| s.desc.code == "full_charge_capacity_mwh")
                    .and_then(|s| s.current.value.as_f64())
                    .unwrap_or(0.0) as u32;

                // 电量百分比:单电池优先,系统级回退
                let dev_pct = status.as_ref().and_then(|st| pct_of_full(st.capacity, full_charge));
                let mut sps = SYSTEM_POWER_STATUS::default();
                let sps_ok = GetSystemPowerStatus(&mut sps).is_ok();
                let pct = dev_pct.or_else(|| {
                    sps_ok
                        .then_some(sps.BatteryLifePercent)
                        .filter(|p| *p <= 100)
                        .map(|p| p as f64)
                });

                if let Some(st) = &status {
                    dev.set_reading("state", Reading::now(Value::Str(state_string(st.power_state, pct).into())));
                    if st.voltage > 0 {
                        dev.set_reading("voltage_mv", Reading::now(Value::U64(st.voltage as u64)));
                    } else {
                        dev.set_reading("voltage_mv", Reading::failed(Status::NotSupported, ValueType::U64));
                    }
                    if st.capacity != BATTERY_UNKNOWN_CAPACITY {
                        dev.set_reading("capacity_mwh", Reading::now(Value::U64(st.capacity as u64)));
                    } else {
                        dev.set_reading("capacity_mwh", Reading::failed(Status::NotSupported, ValueType::U64));
                    }
                    if rate_valid(st.rate) {
                        dev.set_reading("rate_mw", Reading::now(Value::I64(st.rate as i64)));
                    } else {
                        dev.set_reading("rate_mw", Reading::failed(Status::NotSupported, ValueType::I64));
                    }

                    // 预计续航:IOCTL 估计值 > 系统级 > 容量/速率估算
                    if st.power_state & BATTERY_DISCHARGING != 0 {
                        let mut runtime = tag
                            .and_then(|tag| Self::query_info_u32(handle, tag, BATTERY_INFO_ESTIMATED_TIME))
                            .filter(|t| *t != BATTERY_UNKNOWN_TIME && *t > 0)
                            .map(|t| t as f64);
                        if runtime.is_none()
                            && sps_ok
                            && sps.BatteryLifeTime > 0
                            && sps.BatteryLifeTime != BATTERY_UNKNOWN_TIME
                        {
                            runtime = Some(sps.BatteryLifeTime as f64);
                        }
                        if runtime.is_none()
                            && rate_valid(st.rate)
                            && st.rate > 0
                            && st.capacity != BATTERY_UNKNOWN_CAPACITY
                            && st.capacity > 0
                        {
                            runtime = Some(st.capacity as f64 / st.rate as f64 * 3600.0);
                        }
                        match runtime {
                            Some(t) => dev.set_reading("run_time_s", Reading::now(Value::F64(t.round()))),
                            None => dev.set_reading("run_time_s", Reading::failed(Status::NotSupported, ValueType::F64)),
                        }
                    } else {
                        dev.set_reading("run_time_s", Reading::failed(Status::NotSupported, ValueType::F64));
                    }

                    // 充满剩余时间:仅充电中且速率有效(涓流 <100mW 不估算)
                    if st.power_state & BATTERY_CHARGING != 0
                        && rate_valid(st.rate)
                        && st.rate < -100
                        && full_charge > 0
                        && st.capacity != BATTERY_UNKNOWN_CAPACITY
                        && st.capacity < full_charge
                    {
                        let secs = (full_charge - st.capacity) as f64 / (-st.rate) as f64 * 3600.0;
                        dev.set_reading("charge_time_s", Reading::now(Value::F64(secs.round())));
                    } else {
                        dev.set_reading("charge_time_s", Reading::failed(Status::NotSupported, ValueType::F64));
                    }
                } else {
                    dev.set_reading("run_time_s", Reading::failed(Status::NotSupported, ValueType::F64));
                    dev.set_reading("charge_time_s", Reading::failed(Status::NotSupported, ValueType::F64));
                }

                match pct {
                    Some(p) => dev.set_reading("charge_pct", Reading::now(Value::F64(p))),
                    None => dev.set_reading("charge_pct", Reading::failed(Status::NotSupported, ValueType::F64)),
                }
                if sps_ok {
                    dev.set_reading("ac_online", Reading::now(Value::Bool(sps.ACLineStatus == 1)));
                }
                windows::Win32::Foundation::CloseHandle(handle).ok();
            }
        }
    }
}

/// 容量单位:相对容量标记时不是 mWh。
fn cap_unit_of(relative: bool) -> &'static str {
    if relative {
        "rel"
    } else {
        "mWh"
    }
}
