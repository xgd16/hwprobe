//! CPU provider(macOS):识别 + 占用 + 温度(AppleSMC)。
//!
//! 数据源:
//! - 识别:`sysctl`(machdep.cpu.brand_string / hw.physicalcpu / hw.logicalcpu /
//!   hw.perflevel*.physicalcpu 混合架构 P/E 划分 / hw.l2cachesize 等);
//!   NPU 经 IORegistry 的 AppleNeuralEngine 服务判定。
//! - 占用:`host_processor_info` 每核 tick 差分(与 Windows 标准口径同为
//!   "忙碌时间占比",不含频率加权)。
//! - 温度:AppleSMC。Apple Silicon 的逐核温度键随代际变化且**社区映射不完全可靠**
//!   (实测 M2 Pro 上社区表的"P 核键"有 8 个而物理 P 核只有 6 个,且部分键会间歇
//!   返回锁存哨兵值),因此:
//!   1. 逐核温度仅在「表内键数量 == 该簇物理核数」时才声明 —— 数量对不上就
//!      只给整封装温度(CPU 热敏键的最大值),**不硬凑逐核标签**;
//!   2. 读数做合理性过滤(10..115°C),哨兵值不写入;一时读不到时 15 秒内保留
//!      上一个真实读数,超时标 Error。
//! - 频率:Apple Silicon 无任何用户态频率接口(sysctl 连标称值都没有),
//!   不声明频率传感器;Intel Mac 有 `hw.cpufrequency` 标称值,声明
//!   `freq_core_*` 并在名称标注 (Nominal),不冒充实时频率。
//! - 功耗:macOS 用户态无 CPU 封装功耗来源(AS 能耗计数走 IOReport 私有接口,
//!   macOS 27 起还需私有 entitlement),不声明 `power_pkg`。
//!
//! 传感器 code 与 Windows 版(`platform/windows/cpu.rs`)一致:
//! `load_total` / `load_core_{i}` / `freq_core_{i}` / `temperature_pkg` /
//! `temperature_core_{i}`。

use super::smc;
use super::sys;
use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 温度读数合理性区间(°C)。区间外视为 SMC 锁存哨兵值/无效值。
const TEMP_PLAUSIBLE: std::ops::RangeInclusive<f64> = 10.0..=115.0;
/// 上一个真实读数的保鲜期:超过后不再沿用,标 Error。
const TEMP_HOLD: Duration = Duration::from_secs(15);
/// 两次采样间隔超过该值(睡眠/休眠唤醒)时只重建差分基线,不产出数值。
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(10);

/// Apple Silicon 各代 CPU 温度键(社区推断,来源见 docs/smc-keys-research.md;
/// 键的真实存在性在 probe 时逐个核对,数量与物理核数不符就不做逐核标注)。
struct AsTempTable {
    family: &'static str,
    p_cores: &'static [&'static str],
    e_cores: &'static [&'static str],
}

const AS_TEMP_TABLES: &[AsTempTable] = &[
    AsTempTable {
        family: "M1",
        p_cores: &[
            "Tp01", "Tp05", "Tp0D", "Tp0H", "Tp0L", "Tp0P", "Tp0X", "Tp0b",
        ],
        e_cores: &["Tp09", "Tp0T"],
    },
    AsTempTable {
        family: "M2",
        p_cores: &[
            "Tp01", "Tp05", "Tp09", "Tp0D", "Tp0X", "Tp0b", "Tp0f", "Tp0j",
        ],
        e_cores: &["Tp1h", "Tp1t", "Tp1p", "Tp1l"],
    },
    AsTempTable {
        family: "M3",
        p_cores: &[
            "Tf04", "Tf09", "Tf0A", "Tf0B", "Tf0D", "Tf0E", "Tf44", "Tf49", "Tf4A", "Tf4B", "Tf4D",
            "Tf4E",
        ],
        e_cores: &["Te05", "Te0L", "Te0P", "Te0S"],
    },
    AsTempTable {
        family: "M4",
        p_cores: &[
            "Tp01", "Tp05", "Tp09", "Tp0D", "Tp0V", "Tp0Y", "Tp0b", "Tp0e",
        ],
        e_cores: &["Te05", "Te0S", "Te09", "Te0H"],
    },
];

/// Intel Mac CPU 封装温度键(按优先级回退,docs/smc-keys-research.md 多来源一致)。
const INTEL_PKG_KEYS: &[&str] = &["TC0P", "TC0E", "TC0F", "TC0D", "TC0H"];

/// 一个温度读数源的运行期状态。
struct TempSource {
    key: [u8; 4],
    last_good: Option<(f64, Instant)>,
}

/// 逐核温度传感器(仅在键数与物理核数一致时建立)。
struct CoreTemp {
    code: String,
    label: String,
    src: TempSource,
}

struct CpuState {
    /// 上一轮每核 tick 计数([user, system, idle, nice])。
    last_ticks: Option<Vec<[u32; 4]>>,
    last_at: Option<Instant>,
    /// 逐核温度(code/label 对齐 core_labels)。
    core_temps: Vec<CoreTemp>,
    /// 整封装温度的数据源:Intel = 单键;AS = 全部 CPU 热敏键取最大值。
    pkg_sources: Vec<TempSource>,
}

pub struct CpuProvider {
    state: Mutex<CpuState>,
}

impl CpuProvider {
    pub fn new() -> CpuProvider {
        CpuProvider {
            state: Mutex::new(CpuState {
                last_ticks: None,
                last_at: None,
                core_temps: Vec::new(),
                pkg_sources: Vec::new(),
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

/// 从品牌串识别 Apple Silicon 代际("Apple M2 Pro" → "M2")。
fn apple_family(brand: &str) -> Option<&'static str> {
    let b = brand.strip_prefix("Apple ")?;
    for (prefix, family) in [("M1", "M1"), ("M2", "M2"), ("M3", "M3"), ("M4", "M4")] {
        if b.starts_with(prefix) {
            return Some(family);
        }
    }
    None
}

/// 读取一个温度源:合理性过滤 + 短时良值保持。
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

impl Provider for CpuProvider {
    fn describe(&self) -> &str {
        "mac-cpu"
    }

    fn category(&self) -> Category {
        Category::Cpu
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        // ---- 识别(sysctl)----
        let brand =
            sys::sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "Unknown CPU".into());
        let vendor = sys::sysctl_string("machdep.cpu.vendor").unwrap_or_else(|| "Apple".into());
        let is_apple = vendor.eq_ignore_ascii_case("Apple") || brand.starts_with("Apple");
        let cores = sys::sysctl_u64("hw.physicalcpu").unwrap_or(0);
        let logical = sys::sysctl_u64("hw.logicalcpu").unwrap_or(0).max(1) as usize;

        // P/E 划分(hw.perflevel0 = P 簇,perflevel1 = E 簇)
        let p_cores = sys::sysctl_u64("hw.perflevel0.physicalcpu").unwrap_or(0) as usize;
        let e_cores = sys::sysctl_u64("hw.perflevel1.physicalcpu").unwrap_or(0) as usize;

        // 核标签(与 Windows 版同格式:"P-Core #n" / "E-Core #n" / "Core #n")
        let core_labels: Vec<String> = {
            let mut labels = Vec::with_capacity(logical);
            for i in 0..p_cores.min(logical) {
                labels.push(format!("P-Core #{i}"));
            }
            let rest = logical.saturating_sub(labels.len());
            for i in 0..e_cores.min(rest) {
                labels.push(format!("E-Core #{i}"));
            }
            let mut plain = 0;
            while labels.len() < logical {
                labels.push(format!("Core #{plain}"));
                plain += 1;
            }
            labels
        };

        let mut b = DeviceBuilder::new("cpu.package.0", brand.clone())
            .vendor(vendor.clone())
            .model(brand.clone())
            .static_str("brand", "Brand String", &brand)
            .static_f64("cores", "Physical Cores", "", cores as f64)
            .static_f64("logical", "Logical Processors", "", logical as f64);

        if let Some(mhz) = sys::sysctl_u64("hw.cpufrequency").map(|hz| hz / 1_000_000) {
            if mhz > 0 {
                b = b.static_f64("max_clock_mhz", "Max Clock", "MHz", mhz as f64);
            }
        }
        if let Some(bytes) = sys::sysctl_u64("hw.l2cachesize").filter(|v| *v > 0) {
            b = b.static_f64("l2_cache_kb", "L2 Cache", "KB", bytes as f64 / 1024.0);
        }
        if let Some(bytes) = sys::sysctl_u64("hw.l3cachesize").filter(|v| *v > 0) {
            b = b.static_f64("l3_cache_kb", "L3 Cache", "KB", bytes as f64 / 1024.0);
        }
        // NPU:IORegistry 有 ANE 服务才有该字段(不猜型号)
        if super::iokit::matching_service("AppleNeuralEngine").is_some()
            || super::iokit::matching_service("H11ANEIn").is_some()
        {
            b = b.static_str("npu", "Neural Processing Unit", "Apple Neural Engine");
        }

        // ---- 动态:占用 ----
        b = b.dynamic("load_total", "CPU Usage", "%", ValueType::F64);
        for (i, label) in core_labels.iter().enumerate() {
            b = b.dynamic(
                &format!("load_core_{i}"),
                &format!("{label} Usage"),
                "%",
                ValueType::F64,
            );
        }

        // ---- 动态:频率(仅 Intel Mac 的标称值;AS 无任何来源,不声明)----
        if !is_apple {
            if let Some(mhz) = sys::sysctl_u64("hw.cpufrequency").map(|hz| hz / 1_000_000) {
                if mhz > 0 {
                    for (i, label) in core_labels.iter().enumerate() {
                        b = b.dynamic_now(
                            &format!("freq_core_{i}"),
                            &format!("{label} Clock (Nominal)"),
                            "MHz",
                            Value::F64(mhz as f64),
                        );
                    }
                }
            }
        }

        // ---- 动态:温度 ----
        let mut core_temps: Vec<CoreTemp> = Vec::new();
        let mut pkg_sources: Vec<TempSource> = Vec::new();

        if is_apple {
            if let Some(table) =
                apple_family(&brand).and_then(|f| AS_TEMP_TABLES.iter().find(|t| t.family == f))
            {
                let p_found: Vec<[u8; 4]> = table
                    .p_cores
                    .iter()
                    .map(|k| key4(k))
                    .filter(|k| smc::shared(|s| s.key_info(k)).is_some())
                    .collect();
                let e_found: Vec<[u8; 4]> = table
                    .e_cores
                    .iter()
                    .map(|k| key4(k))
                    .filter(|k| smc::shared(|s| s.key_info(k)).is_some())
                    .collect();

                // 逐核标签的诚实性前提:表内键数量与该簇物理核数严格一致。
                let per_core_ok =
                    !p_found.is_empty() && p_found.len() == p_cores && e_found.len() == e_cores;
                if per_core_ok {
                    for (i, label) in core_labels.iter().enumerate() {
                        let key = if i < p_cores {
                            p_found.get(i).copied()
                        } else {
                            e_found.get(i - p_cores).copied()
                        };
                        if let Some(k) = key {
                            core_temps.push(CoreTemp {
                                code: format!("temperature_core_{i}"),
                                label: label.clone(),
                                src: TempSource {
                                    key: k,
                                    last_good: None,
                                },
                            });
                        }
                    }
                } else {
                    crate::logbuf::info(format!(
                        "CPU: 温度键数量(P {}/E {})与物理核数(P {}/E {})不一致,\
                         不做逐核标注,只给整封装温度(键表 {},社区推断)",
                        p_found.len(),
                        e_found.len(),
                        p_cores,
                        e_cores,
                        table.family
                    ));
                }
                // 整封装温度 = 该代 CPU 热敏键的最大值
                for k in p_found.into_iter().chain(e_found) {
                    pkg_sources.push(TempSource {
                        key: k,
                        last_good: None,
                    });
                }
            }
        } else {
            // Intel Mac:TC0P 系列封装温度 + TC*c 逐核
            if let Some(k) = INTEL_PKG_KEYS
                .iter()
                .map(|n| key4(n))
                .find(|k| smc::shared(|s| s.key_info(k)).is_some())
            {
                pkg_sources.push(TempSource {
                    key: k,
                    last_good: None,
                });
            }
            let per_core: Vec<[u8; 4]> = (0..10)
                .map(|i| key4(&format!("TC{i}c")))
                .filter(|k| smc::shared(|s| s.key_info(k)).is_some())
                .collect();
            if per_core.len() == cores as usize {
                for (i, label) in core_labels.iter().enumerate() {
                    if let Some(k) = per_core.get(i) {
                        core_temps.push(CoreTemp {
                            code: format!("temperature_core_{i}"),
                            label: label.clone(),
                            src: TempSource {
                                key: *k,
                                last_good: None,
                            },
                        });
                    }
                }
            }
        }

        if !pkg_sources.is_empty() {
            b = b.dynamic(
                "temperature_pkg",
                "CPU Package Temperature",
                "°C",
                ValueType::F64,
            );
        }
        for ct in &core_temps {
            b = b.dynamic(
                &ct.code,
                &format!("{} Temperature", ct.label),
                "°C",
                ValueType::F64,
            );
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.core_temps = core_temps;
        state.pkg_sources = pkg_sources;

        Ok(vec![b.build()])
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let Some(dev) = devices.first_mut() else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // ---- 占用:tick 差分 ----
        if let Some(ticks) = sys::cpu_ticks() {
            let now = Instant::now();
            let gap_ok = state
                .last_at
                .map(|t| now.duration_since(t) <= MAX_SAMPLE_GAP)
                .unwrap_or(false);
            match (state.last_ticks.as_ref(), gap_ok) {
                (Some(prev), true) if prev.len() == ticks.len() => {
                    let mut total_busy = 0.0f64;
                    let mut total_all = 0.0f64;
                    for (i, (p, c)) in prev.iter().zip(ticks.iter()).enumerate() {
                        // user + system + nice 为忙碌;计数器可能回绕,用 wrapping 差
                        let busy = (c[0].wrapping_sub(p[0]))
                            .wrapping_add(c[1].wrapping_sub(p[1]))
                            .wrapping_add(c[3].wrapping_sub(p[3]));
                        let idle = c[2].wrapping_sub(p[2]);
                        let all = busy as f64 + idle as f64;
                        if all > 0.0 {
                            let pct = (busy as f64 / all * 100.0).clamp(0.0, 100.0);
                            dev.set_reading(
                                &format!("load_core_{i}"),
                                Reading::now(Value::F64(round1(pct))),
                            );
                            total_busy += busy as f64;
                            total_all += all;
                        }
                    }
                    if total_all > 0.0 {
                        let pct = (total_busy / total_all * 100.0).clamp(0.0, 100.0);
                        dev.set_reading("load_total", Reading::now(Value::F64(round1(pct))));
                    }
                }
                (Some(_), false) => {
                    // 跨睡眠/长时间断档:本轮只重建基线,不产出数值(避免假峰值)
                    crate::logbuf::info("CPU: 采样间隔过长,重建差分基线(本轮不出值)");
                }
                _ => {}
            }
            state.last_ticks = Some(ticks);
            state.last_at = Some(now);
        } else {
            dev.set_reading("load_total", Reading::failed(Status::Error, ValueType::F64));
        }

        // ---- 温度 ----
        let now = Instant::now();
        let mut pkg_max: Option<f64> = None;

        for src in state.pkg_sources.iter_mut() {
            if let Some(v) = read_temp(src, now) {
                pkg_max = Some(pkg_max.map_or(v, |m: f64| m.max(v)));
            }
        }
        // set_reading 对未声明的 code 是 no-op,这里直接写即可
        match pkg_max {
            Some(v) => dev.set_reading("temperature_pkg", Reading::now(Value::F64(round1(v)))),
            None => dev.set_reading(
                "temperature_pkg",
                Reading::failed(Status::Error, ValueType::F64),
            ),
        }

        for ct in state.core_temps.iter_mut() {
            match read_temp(&mut ct.src, now) {
                Some(v) => dev.set_reading(&ct.code, Reading::now(Value::F64(round1(v)))),
                None => dev.set_reading(&ct.code, Reading::failed(Status::Error, ValueType::F64)),
            }
        }
    }
}

/// 保留一位小数(与 Windows 输出口径一致)。
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}
