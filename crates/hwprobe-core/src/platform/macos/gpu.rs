//! GPU provider(macOS):IOKit `PerformanceStatistics` + AppleSMC 温度。
//!
//! 数据源:
//! - 枚举:`IOServiceMatching("IOAccelerator")`(Apple Silicon 为 AGXAccelerator*
//!   子类,Intel 为 IOGFXAccelerator / AMDRadeon* / IntelAccelerator);
//! - 利用率/显存:节点属性 `PerformanceStatistics` 字典(本机 AGX 实测键:
//!   `Device Utilization %` / `Renderer Utilization %` / `Tiler Utilization %` /
//!   `In use system memory`(字节,可超 2^31,按 64 位读)/ `Alloc system memory`);
//!   Intel Mac 另有 `GPU Activity(%)`;
//! - 温度:AppleSMC `Tg*`/`TG*` 键(仅单 GPU 机器归属到该卡;多卡时 SMC 键
//!   无法归属到具体卡,不猜);
//! - 静态:`model` / `vendor-id` / `device-id` / `IOSourceVersion`(驱动版本)/
//!   `gpu-core-count`。
//!
//! 如实省略(Apple Silicon 无对应数据源):
//! - 分引擎占用(`util_eng_*` 的数据源是 Windows GPU Engine 计数器);
//! - 功耗/风扇/核心显存频率(AS 走 IOReport 私有接口,需 entitlement);
//! - `util_media_pct`(AGX 的 Tiler 不是媒体引擎,语义不对应,不冒充)。

use super::iokit::{self, CfDict, IoObject};
use super::smc;
use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// GPU 温度合理性区间(°C)。
const TEMP_PLAUSIBLE: std::ops::RangeInclusive<f64> = 10.0..=115.0;
const TEMP_HOLD: Duration = Duration::from_secs(15);

/// GPU 温度 SMC 键(社区推断,probe 时核对存在性)。
struct GpuTempTable {
    family: &'static str,
    keys: &'static [&'static str],
}

const AS_GPU_TEMP_TABLES: &[GpuTempTable] = &[
    GpuTempTable {
        family: "M1",
        keys: &["Tg05", "Tg0D", "Tg0L", "Tg0T"],
    },
    GpuTempTable {
        family: "M2",
        keys: &["Tg0f", "Tg0j"],
    },
    GpuTempTable {
        family: "M3",
        keys: &[
            "Tf14", "Tf18", "Tf19", "Tf1A", "Tf24", "Tf28", "Tf29", "Tf2A",
        ],
    },
    GpuTempTable {
        family: "M4",
        keys: &[
            "Tg0G", "Tg0H", "Tg1U", "Tg1k", "Tg0K", "Tg0L", "Tg0d", "Tg0e", "Tg0j", "Tg0k",
        ],
    },
];

const INTEL_GPU_TEMP_KEYS: &[&str] = &["TG0P", "TG0D", "TGDD", "TG0H"];

struct TempSource {
    key: [u8; 4],
    last_good: Option<(f64, Instant)>,
}

struct GpuState {
    entry: IoObject,
    temp_sources: Vec<TempSource>,
}

pub struct GpuProvider {
    state: Mutex<Vec<GpuState>>,
}

impl GpuProvider {
    pub fn new() -> GpuProvider {
        GpuProvider {
            state: Mutex::new(Vec::new()),
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

fn read_temp(src: &mut TempSource, now: Instant) -> Option<f64> {
    let raw = smc::shared(|s| s.read_f64(&src.key));
    if let Some(v) = raw.filter(|v| TEMP_PLAUSIBLE.contains(v)) {
        src.last_good = Some((v, now));
        return Some(v);
    }
    src.last_good
        .filter(|(_, at)| now.duration_since(*at) < TEMP_HOLD)
        .map(|(v, _)| v)
}

/// vendor-id 属性(小端 4 字节 CFData)→ 厂商名。
fn vendor_name(props: &CfDict) -> String {
    let id = iokit::dict_value(props, "vendor-id")
        .and_then(iokit::cf_data)
        .and_then(|d| {
            let b = d.get(0..4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        })
        .unwrap_or(0);
    match id & 0xFFFF {
        0x106B => "Apple".to_string(),
        0x1002 => "AMD".to_string(),
        0x8086 => "Intel".to_string(),
        0x10DE => "NVIDIA".to_string(),
        0 => "Unknown".to_string(),
        other => format!("VID {other:04X}"),
    }
}

/// vendor-id / device-id 拼成 "vvvv:dddd"(与 Windows 版 device_id 格式一致)。
fn device_id(props: &CfDict) -> Option<String> {
    let le_u32 = |key: &str| -> Option<u32> {
        let d = iokit::dict_value(props, key).and_then(iokit::cf_data)?;
        let b = d.get(0..4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let vendor = le_u32("vendor-id")? & 0xFFFF;
    let device = le_u32("device-id")? & 0xFFFF;
    Some(format!("{vendor:04X}:{device:04X}"))
}

impl Provider for GpuProvider {
    fn describe(&self) -> &str {
        "mac-gpu"
    }

    fn category(&self) -> Category {
        Category::Gpu
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let entries = iokit::matching_services("IOAccelerator");
        if entries.is_empty() {
            return Err("IORegistry 中没有 IOAccelerator 实例".into());
        }
        let single_gpu = entries.len() == 1;

        // Apple Silicon 代际(供 GPU 温度键表选择)
        let brand = super::sys::sysctl_string("machdep.cpu.brand_string").unwrap_or_default();
        let family = brand
            .strip_prefix("Apple M")
            .and_then(|b| b.chars().next())
            .map(|c| format!("M{c}"));

        let mut devices = Vec::new();
        let mut states = Vec::new();

        for entry in entries {
            let Some(props) = iokit::registry_props(&entry) else {
                continue;
            };
            let name = iokit::dict_string(&props, "model").unwrap_or_else(|| "Apple GPU".into());
            let vendor = vendor_name(&props);
            let id = match iokit::entry_id(&entry) {
                Some(eid) => format!("gpu.{eid:x}"),
                None => format!("gpu.{}", devices.len()),
            };

            let mut b = DeviceBuilder::new(id, name.clone())
                .vendor(vendor)
                .model(name);
            if let Some(did) = device_id(&props) {
                b = b.static_str("device_id", "PCI Device ID", did);
            }
            if let Some(v) = iokit::dict_string(&props, "IOSourceVersion") {
                b = b.static_str("driver_version", "Driver Version", v);
            }
            if let Some(n) = iokit::dict_i64(&props, "gpu-core-count") {
                if n > 0 {
                    b = b.static_f64("core_count", "GPU Cores", "", n as f64);
                }
            }
            // VRAM 总量:Intel Mac 的 PCI 显示节点有 "VRAM,totalMB";AS 统一内存无此概念
            let vram_total_mb = iokit::dict_i64(&props, "VRAM,totalMB")
                .or_else(|| {
                    let d = iokit::dict_value(&props, "VRAM,totalMB").and_then(iokit::cf_data)?;
                    let bytes = d.get(0..4)?;
                    Some(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64)
                })
                .filter(|v| *v > 0);
            if let Some(mb) = vram_total_mb {
                b = b.static_f64("vram_total_mb", "VRAM Total", "MB", mb as f64);
            }

            // 动态传感器
            b = b.dynamic("util_gpu_pct", "GPU Usage", "%", ValueType::F64);
            b = b.dynamic("mem_used_mb", "VRAM Used", "MB", ValueType::F64);
            if vram_total_mb.is_some() {
                b = b.dynamic("mem_used_pct", "VRAM Usage", "%", ValueType::F64);
            }
            // Renderer 利用率:AGX 有 "Renderer Utilization %",部分 Intel/AMD 有
            // "Renderer Utilization %";都没有则不声明。
            if iokit::nested_value(&props, "PerformanceStatistics", "Renderer Utilization %")
                .is_some()
                || iokit::nested_value(&props, "PerformanceStatistics", "Renderer Utilization")
                    .is_some()
            {
                b = b.dynamic("util_render_pct", "Render Usage", "%", ValueType::F64);
            }

            // 温度:SMC。单 GPU 机器才归属到该卡(多卡时 SMC 键无法归属,不猜)。
            let mut temp_sources: Vec<TempSource> = Vec::new();
            if single_gpu {
                let keys: Vec<[u8; 4]> = match family.as_deref() {
                    Some(f) => AS_GPU_TEMP_TABLES
                        .iter()
                        .find(|t| t.family == f)
                        .map(|t| {
                            t.keys
                                .iter()
                                .map(|k| key4(k))
                                .filter(|k| smc::shared(|s| s.key_info(k)).is_some())
                                .collect()
                        })
                        .unwrap_or_default(),
                    None => INTEL_GPU_TEMP_KEYS
                        .iter()
                        .map(|k| key4(k))
                        .filter(|k| smc::shared(|s| s.key_info(k)).is_some())
                        .take(1)
                        .collect(),
                };
                if !keys.is_empty() {
                    b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64);
                    temp_sources = keys
                        .into_iter()
                        .map(|k| TempSource {
                            key: k,
                            last_good: None,
                        })
                        .collect();
                }
            }

            devices.push(b.build());
            states.push(GpuState {
                entry,
                temp_sources,
            });
        }

        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = states;
        Ok(devices)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let mut states = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        for (i, st) in states.iter_mut().enumerate() {
            let Some(dev) = devices.get_mut(i) else { break };
            let Some(props) = iokit::registry_props(&st.entry) else {
                dev.set_reading(
                    "util_gpu_pct",
                    Reading::failed(Status::Error, ValueType::F64),
                );
                continue;
            };

            // 利用率:AGX 用 "Device Utilization %",Intel/AMD 可能是 "GPU Activity(%)"
            let util = iokit::nested_i64(&props, "PerformanceStatistics", "Device Utilization %")
                .or_else(|| iokit::nested_i64(&props, "PerformanceStatistics", "GPU Activity(%)"))
                .map(|v| (v.clamp(0, 100)) as f64);
            match util {
                Some(v) => dev.set_reading("util_gpu_pct", Reading::now(Value::F64(v))),
                None => dev.set_reading(
                    "util_gpu_pct",
                    Reading::failed(Status::Error, ValueType::F64),
                ),
            }

            if let Some(render) =
                iokit::nested_i64(&props, "PerformanceStatistics", "Renderer Utilization %")
                    .or_else(|| {
                        iokit::nested_i64(&props, "PerformanceStatistics", "Renderer Utilization")
                    })
                    .map(|v| v.clamp(0, 100) as f64)
            {
                dev.set_reading("util_render_pct", Reading::now(Value::F64(render)));
            }

            // 显存占用("In use system memory" 为字节)
            match iokit::nested_i64(&props, "PerformanceStatistics", "In use system memory") {
                Some(bytes) if bytes >= 0 => {
                    let mb = bytes as f64 / (1024.0 * 1024.0);
                    dev.set_reading("mem_used_mb", Reading::now(Value::F64(mb.round())));
                    if let (Some(total), true) =
                        (iokit::dict_i64(&props, "VRAM,totalMB"), bytes > 0)
                    {
                        if total > 0 {
                            let pct = (bytes as f64 / (total as f64 * 1024.0 * 1024.0) * 100.0)
                                .clamp(0.0, 100.0);
                            dev.set_reading(
                                "mem_used_pct",
                                Reading::now(Value::F64((pct * 10.0).round() / 10.0)),
                            );
                        }
                    }
                }
                _ => {
                    dev.set_reading(
                        "mem_used_mb",
                        Reading::failed(Status::Error, ValueType::F64),
                    );
                }
            }

            // 温度(SMC,取各键最大值;合理性过滤 + 15s 良值保持)
            if !st.temp_sources.is_empty() {
                let mut max_t: Option<f64> = None;
                for src in st.temp_sources.iter_mut() {
                    if let Some(v) = read_temp(src, now) {
                        max_t = Some(max_t.map_or(v, |m: f64| m.max(v)));
                    }
                }
                match max_t {
                    Some(v) => dev.set_reading(
                        "temperature_c",
                        Reading::now(Value::F64((v * 10.0).round() / 10.0)),
                    ),
                    None => dev.set_reading(
                        "temperature_c",
                        Reading::failed(Status::Error, ValueType::F64),
                    ),
                }
            }
        }
    }
}
