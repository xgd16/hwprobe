//! MSI Claw 掌机 EC 传感器:风扇转速 / CPU·GPU 温度 / 性能模式 / 电池充电
//! 上限 / 功率限制读回,经 ACPI EC(见 device_profile 的寄存器表)。
//!
//! 挂在主板类别下作为第二设备("Embedded Controller (EC)")。
//!
//! 校验策略(绝不给假数据):probe 时逐寄存器做范围校验,通过一项才声明
//! 一项传感器;refresh 时同样范围校验,越界读值标记 Error 而非输出垃圾值。

#![cfg(target_os = "windows")]

use crate::model::{DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::platform::windows::{device_profile, driver};
use std::sync::Mutex;

/// 转速合理上限(掌机单风扇 < 8000 RPM)。
const RPM_MAX: f64 = 8000.0;
/// 功率限制合理范围(W)。
const POWER_LIMIT_MAX: f64 = 64.0;

const DEVICE_ID: &str = "mb-ec:msi-claw";

pub struct ClawEc {
    ec_cell: Mutex<Option<driver::AcpiEc>>,
}

impl ClawEc {
    pub fn new() -> ClawEc {
        ClawEc {
            ec_cell: Mutex::new(None),
        }
    }

    /// 探测:机型无表 / 无驱动 / EC 不可达 / 校验全不过 → None。
    pub fn probe(&self) -> Option<DeviceData> {
        let profile = device_profile::detect();
        let Some(map) = profile.ec_map else {
            crate::logbuf::info("非适配机型,EC 传感器跳过");
            return None;
        };

        let ec = match driver::AcpiEc::open() {
            Some(ec) => ec,
            None => {
                crate::logbuf::info("ACPI EC 不可用(需驱动层),EC 传感器跳过");
                return None;
            }
        };

        // ---- 逐寄存器校验,通过才声明对应传感器 ----
        let mut validated: Vec<(&'static str, &'static str, &'static str, ValueType)> =
            Vec::new();

        if ec.read(map.cpu_temp).is_some_and(|v| (5..=105).contains(&v)) {
            validated.push(("temperature_cpu_c", "CPU Temperature (EC)", "°C", ValueType::F64));
        }
        if ec.read(map.gpu_temp).is_some_and(|v| (5..=110).contains(&v)) {
            validated.push(("temperature_gpu_c", "GPU Temperature (EC)", "°C", ValueType::F64));
        }
        if read_rpm(&ec, map).is_some() {
            validated.push(("fan_rpm", "Fan Speed (EC)", "RPM", ValueType::F64));
        }
        if ec.read(map.performance_mode).is_some() {
            validated.push(("performance_mode", "Performance Mode", "", ValueType::Str));
            validated.push(("performance_mode_raw", "Performance Mode (raw)", "", ValueType::U64));
        }
        if ec.read(map.fan_override).is_some() {
            validated.push(("fan_override", "Fan Override (software)", "", ValueType::Bool));
        }
        if ec.read(map.cooler_boost).is_some() {
            validated.push(("cooler_boost", "Cooler Boost (full speed)", "", ValueType::Bool));
        }
        if ec.read(map.charge_limit).is_some() {
            validated.push(("charge_limit_pct", "Battery Charge Limit", "%", ValueType::F64));
            validated.push(("charge_limit_enabled", "Charge Limit Enabled", "", ValueType::Bool));
        }
        // 功耗限制寄存器(0x50/0x51)的出处只有 HandheldCompanion 的 WMI 块号
        // 推断,msi-ec 主线并无此表 —— 单靠 2..64W 的范围校验不足以排除
        // "错地址恰好读出范围内的值"。CPU 功耗墙在 Intel 平台可经 RAPL
        // (MSR 0x610)独立读出,两者应一致:偏差 > 3W 判为寄存器语义不符,
        // 不下发传感器(MSR 不可读时保留原范围校验)。
        let msr_limits = driver::read_package_power_limits();

        let spl_raw = ec.read(map.power_limit_spl);
        let spl_in_range = spl_raw.is_some_and(|v| (2.0..=POWER_LIMIT_MAX).contains(&(v as f64)));
        let spl_ok = match msr_limits {
            Some((pl1, _)) if spl_in_range => {
                spl_raw.is_some_and(|v| (v as f64 - pl1).abs() <= 3.0)
            }
            _ => spl_in_range,
        };
        if spl_in_range && !spl_ok {
            crate::logbuf::warn("EC 0x50 与 MSR PL1 偏差过大,SPL 传感器未下发(寄存器语义可能不符)");
        }
        if spl_ok {
            validated.push(("power_limit_spl_w", "Power Limit SPL/PL1", "W", ValueType::F64));
        }

        let sppt_raw = ec.read(map.power_limit_sppt);
        let sppt_in_range = sppt_raw.is_some_and(|v| (2.0..=POWER_LIMIT_MAX).contains(&(v as f64)));
        let sppt_ok = if sppt_in_range {
            match msr_limits.and_then(|(_, pl2)| pl2) {
                Some(pl2) => {
                    let ok = sppt_raw.is_some_and(|v| (v as f64 - pl2).abs() <= 3.0);
                    if !ok {
                        crate::logbuf::warn(
                            "EC 0x51 与 MSR PL2 偏差过大,sPPT 传感器未下发(寄存器语义可能不符)",
                        );
                    }
                    ok
                }
                // 无 MSR PL2 参照(域未实现/不可读):保留范围校验结果
                None => true,
            }
        } else {
            false
        };
        if sppt_ok {
            validated.push(("power_limit_sppt_w", "Power Limit sPPT/PL2", "W", ValueType::F64));
        }

        if validated.is_empty() {
            crate::logbuf::warn("EC 可达但没有寄存器通过校验,不下发任何传感器");
            return None;
        }
        crate::logbuf::info(format!(
            "{} EC 传感器就绪:{} 项寄存器通过校验",
            profile.marketing_name,
            validated.len()
        ));

        let mut b = DeviceBuilder::new(DEVICE_ID, "Embedded Controller (EC)")
            .vendor("Micro-Star International")
            .model(profile.marketing_name)
            .static_str("ec_map_source", "EC Map Source", profile.ec_map_note);

        for (code, name, unit, vtype) in validated {
            b = b.dynamic(code, name, unit, vtype);
        }

        *self.ec_cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(ec);
        Some(b.build())
    }

    /// 刷新 EC 设备(probe 未就绪或设备不在列表中时为空操作)。
    pub fn refresh(&self, devices: &mut [DeviceData]) {
        let ec_cell = self.ec_cell.lock().unwrap_or_else(|e| e.into_inner());
        let Some(ec) = ec_cell.as_ref() else {
            return;
        };
        let Some(map) = device_profile::detect().ec_map else {
            return;
        };
        let Some(dev) = devices.iter_mut().find(|d| d.meta.id == DEVICE_ID) else {
            return;
        };

        // 温度:越界一律 Error,不输出垃圾值
        set_temp(ec, dev, "temperature_cpu_c", map.cpu_temp, 5..=105);
        set_temp(ec, dev, "temperature_gpu_c", map.gpu_temp, 5..=110);

        match read_rpm(ec, map) {
            Some(rpm) if rpm <= RPM_MAX => dev.set_reading("fan_rpm", Reading::now(Value::F64(rpm))),
            _ => dev.set_reading("fan_rpm", Reading::failed(Status::Error, ValueType::F64)),
        }

        match ec.read(map.performance_mode) {
            Some(raw) => {
                let mode = raw & 0x3F;
                let name = match mode {
                    0 => "Comfort".to_string(),
                    1 => "Green".to_string(),
                    2 => "ECO".to_string(),
                    3 => "User".to_string(),
                    4 => "Sport".to_string(),
                    other => format!("Mode {other}"),
                };
                dev.set_reading("performance_mode", Reading::now(Value::Str(name)));
                dev.set_reading("performance_mode_raw", Reading::now(Value::U64(mode as u64)));
            }
            None => {
                dev.set_reading(
                    "performance_mode",
                    Reading::failed(Status::Error, ValueType::Str),
                );
                dev.set_reading(
                    "performance_mode_raw",
                    Reading::failed(Status::Error, ValueType::U64),
                );
            }
        }

        set_flag(ec, dev, "fan_override", map.fan_override, 0x80);
        set_flag(ec, dev, "cooler_boost", map.cooler_boost, 0x80);

        match ec.read(map.charge_limit) {
            Some(raw) => {
                dev.set_reading("charge_limit_pct", Reading::now(Value::F64((raw & 0x7F) as f64)));
                dev.set_reading(
                    "charge_limit_enabled",
                    Reading::now(Value::Bool(raw & 0x80 != 0)),
                );
            }
            None => {
                dev.set_reading("charge_limit_pct", Reading::failed(Status::Error, ValueType::F64));
                dev.set_reading(
                    "charge_limit_enabled",
                    Reading::failed(Status::Error, ValueType::Bool),
                );
            }
        }

        set_power_limit(ec, dev, "power_limit_spl_w", map.power_limit_spl);
        set_power_limit(ec, dev, "power_limit_sppt_w", map.power_limit_sppt);
    }
}

fn set_temp(
    ec: &driver::AcpiEc,
    dev: &mut DeviceData,
    code: &str,
    addr: u8,
    range: std::ops::RangeInclusive<u8>,
) {
    match ec.read(addr).filter(|v| range.contains(v)) {
        Some(v) => dev.set_reading(code, Reading::now(Value::F64(v as f64))),
        None => dev.set_reading(code, Reading::failed(Status::Error, ValueType::F64)),
    }
}

fn set_flag(ec: &driver::AcpiEc, dev: &mut DeviceData, code: &str, addr: u8, bit: u8) {
    match ec.read(addr) {
        Some(raw) => dev.set_reading(code, Reading::now(Value::Bool(raw & bit != 0))),
        None => dev.set_reading(code, Reading::failed(Status::Error, ValueType::Bool)),
    }
}

fn set_power_limit(ec: &driver::AcpiEc, dev: &mut DeviceData, code: &str, addr: u8) {
    match ec
        .read(addr)
        .filter(|v| (2.0..=POWER_LIMIT_MAX).contains(&(*v as f64)))
    {
        Some(v) => dev.set_reading(code, Reading::now(Value::F64(v as f64))),
        None => dev.set_reading(code, Reading::failed(Status::Error, ValueType::F64)),
    }
}

/// 读风扇转速:0x71 低字节 + 0x72 高字节(小端 16 位)。
/// 高字节不合理(> 0x1F ≈ 8192 RPM)时判为寄存器语义不符。
fn read_rpm(ec: &driver::AcpiEc, map: &device_profile::EcRegisterMap) -> Option<f64> {
    let lo = ec.read(map.fan_rpm_lo)?;
    let hi = ec.read(map.fan_rpm_hi)?;
    if hi > 0x1F {
        return None;
    }
    Some((((hi as u16) << 8) | lo as u16) as f64)
}
