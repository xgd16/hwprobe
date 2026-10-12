//! Motherboard provider(macOS):平台识别 + SMC 风扇/板载温度/整机功耗。
//!
//! 数据源:
//! - 识别:`IOPlatformExpertDevice`(model 型号标识 "Mac14,9"、
//!   IOPlatformSerialNumber 序列号、model-number 机型号);Boot ROM 版本在
//!   macOS 上没有用户态来源(system_profiler 用私有 SPI),**如实省略**;
//! - 风扇:SMC `FNum`(数量)+ `F{i}Ac`(实际转速,Intel 为 fpe2、Apple Silicon
//!   实测为 flt,按 key_info 类型自适应解码);无风扇机型(MacBook Air)不声明;
//! - 板载温度:SMC 中有公开文档语义的键(`TW0P` WiFi 模块),实测核对存在才声明;
//!   语义不明的键(TVM*/TRD*/TPD* 等,实测存在但无可靠文档)**不用**;
//! - 功耗:`PSTR` 整机功耗(W)与 `PDTR` DC-In 输入功率(W),来源 macmon 实测
//!   + 本机负载实验验证(随 CPU 负载响应);CPU/GPU 分项功耗无用户态来源,不声明。
//!
//! 传感器 code 沿用 Windows 版 SuperIO 的通用通道命名(`fan_{i}` /
//! `temperature_{i}`);功耗是 mac 新增 code(`power_total_w` / `power_dcin_w`),
//! 仅在键存在时声明。

use super::iokit;
use super::smc;
use super::sys;
use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 板载温度键(键 → 传感器名)。只收录有公开语义且本机实测存在的键。
const BOARD_TEMP_KEYS: &[(&str, &str)] = &[("TW0P", "WiFi Module Temperature")];

/// 功耗键(键 → (code, 名称))。
const POWER_KEYS: &[(&str, &str, &str)] = &[
    ("PSTR", "power_total_w", "Total System Power"),
    ("PDTR", "power_dcin_w", "DC-In Power"),
];

const TEMP_PLAUSIBLE: std::ops::RangeInclusive<f64> = 10.0..=115.0;
const TEMP_HOLD: Duration = Duration::from_secs(15);

struct TempSource {
    code: String,
    key: [u8; 4],
    last_good: Option<(f64, Instant)>,
}

struct FanSource {
    code: String,
    key: [u8; 4],
}

struct PowerSource {
    code: String,
    key: [u8; 4],
}

struct BoardState {
    fans: Vec<FanSource>,
    temps: Vec<TempSource>,
    powers: Vec<PowerSource>,
}

pub struct MotherboardProvider {
    state: Mutex<BoardState>,
}

impl MotherboardProvider {
    pub fn new() -> MotherboardProvider {
        MotherboardProvider {
            state: Mutex::new(BoardState {
                fans: Vec::new(),
                temps: Vec::new(),
                powers: Vec::new(),
            }),
        }
    }
}

fn key4(name: &str) -> [u8; 4] {
    let mut k = [0u8; 4];
    let b = name.as_bytes();
    let n = b.len().min(4);
    k[..n].copy_from_slice(&b[..n]);
    k
}

impl Provider for MotherboardProvider {
    fn describe(&self) -> &str {
        "mac-motherboard"
    }

    fn category(&self) -> Category {
        Category::Motherboard
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1500)
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        // ---- 识别(IOPlatformExpertDevice)----
        let Some(platform) = iokit::matching_service("IOPlatformExpertDevice") else {
            return Err("IORegistry 中没有 IOPlatformExpertDevice".into());
        };
        // "model" 是 CFData("Mac14,9"),IOPlatformSerialNumber 是 CFString
        let model_id = iokit::entry_prop(&platform, "model", iokit::cf_data)
            .and_then(|d| {
                let s: Vec<u8> = d.iter().copied().filter(|b| *b != 0).collect();
                String::from_utf8(s).ok()
            })
            .or_else(|| sys::sysctl_string("hw.model"))
            .unwrap_or_else(|| "Mac".into());
        let serial = iokit::entry_prop(&platform, "IOPlatformSerialNumber", iokit::cf_string)
            .unwrap_or_default();
        let model_number = iokit::entry_prop(&platform, "model-number", iokit::cf_data)
            .and_then(|d| {
                let s: Vec<u8> = d.iter().copied().filter(|b| *b != 0).collect();
                String::from_utf8(s).ok()
            })
            .unwrap_or_default();

        let mut b = DeviceBuilder::new("motherboard.0", model_id.clone())
            .vendor("Apple")
            .model(model_id.clone())
            .static_str("system_manufacturer", "System Manufacturer", "Apple")
            .static_str("system_product", "System Product", &model_id);
        if !serial.is_empty() {
            b = b.serial(serial);
        }
        if !model_number.is_empty() {
            b = b.static_str("model_number", "Model Number", &model_number);
        }

        // ---- 风扇(SMC FNum + F{i}Ac)----
        let fan_count = smc::read_key("FNum")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as usize;
        let mut fans = Vec::new();
        for i in 0..fan_count.min(8) {
            let ac = format!("F{i}Ac");
            if smc::key_exists(&ac) {
                b = b.dynamic(
                    &format!("fan_{i}"),
                    &format!("Fan #{i}"),
                    "RPM",
                    ValueType::F64,
                );
                fans.push(FanSource {
                    code: format!("fan_{i}"),
                    key: key4(&ac),
                });
            }
        }

        // ---- 板载温度(仅公开语义键)----
        let mut temps = Vec::new();
        for (kn, name) in BOARD_TEMP_KEYS {
            if smc::key_exists(kn) {
                let code = format!("temperature_{}", temps.len());
                b = b.dynamic(&code, name, "°C", ValueType::F64);
                temps.push(TempSource {
                    code,
                    key: key4(kn),
                    last_good: None,
                });
            }
        }

        // ---- 功耗(仅存在才声明)----
        let mut powers = Vec::new();
        for (kn, code, name) in POWER_KEYS {
            if smc::key_exists(kn) {
                b = b.dynamic(code, name, "W", ValueType::F64);
                powers.push(PowerSource {
                    code: (*code).into(),
                    key: key4(kn),
                });
            }
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.fans = fans;
        state.temps = temps;
        state.powers = powers;

        Ok(vec![b.build()])
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let Some(dev) = devices.first_mut() else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        for fan in &state.fans {
            // 风扇 0 RPM 是合法值(停转),只有读失败才标错
            match smc::shared(|s| s.read_f64(&fan.key)).filter(|v| (0.0..=30000.0).contains(v)) {
                Some(rpm) => dev.set_reading(&fan.code, Reading::now(Value::F64(rpm.round()))),
                None => dev.set_reading(&fan.code, Reading::failed(Status::Error, ValueType::F64)),
            }
        }

        for t in state.temps.iter_mut() {
            let raw = smc::shared(|s| s.read_f64(&t.key));
            let good = raw.filter(|v| TEMP_PLAUSIBLE.contains(v));
            let value = match good {
                Some(v) => {
                    t.last_good = Some((v, now));
                    Some(v)
                }
                None => t
                    .last_good
                    .filter(|(_, at)| now.duration_since(*at) < TEMP_HOLD)
                    .map(|(v, _)| v),
            };
            match value {
                Some(v) => {
                    dev.set_reading(&t.code, Reading::now(Value::F64((v * 10.0).round() / 10.0)))
                }
                None => dev.set_reading(&t.code, Reading::failed(Status::Error, ValueType::F64)),
            }
        }

        for p in &state.powers {
            match smc::shared(|s| s.read_f64(&p.key)).filter(|v| (0.0..=2000.0).contains(v)) {
                Some(w) => {
                    dev.set_reading(&p.code, Reading::now(Value::F64((w * 10.0).round() / 10.0)))
                }
                None => dev.set_reading(&p.code, Reading::failed(Status::Error, ValueType::F64)),
            }
        }
    }
}
