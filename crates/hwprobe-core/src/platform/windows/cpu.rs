//! CPU provider:识别(CPUID + WMI)+ 动态(占用/频率/温度/功耗)。
//!
//! - 总占用与逐核占用:两种口径,由 `options::set_usage_mode` 全局切换
//!   · 标准口径(默认):GetSystemTimes / NtQuerySystemInformation 差分
//!     (不依赖 PDH,规避部分机器上性能计数器被安全软件干扰的问题)
//!   · 任务管理器口径:PDH Utility÷Performance 归一化(同 Win11 24H2+
//!     任务管理器默认视图;数学上等价于时间占比,PDH 不可用时该口径报 Error)
//! - 逐核频率:PDH Actual Frequency(实时反映睿频/降频,两种口径一致);
//!   PDH 不可用时回退 CallNtPowerInformation(仅标称频率,不随负载变化)
//! - 混合架构拓扑:CPUID leaf 0x1A 逐核区分 P/E 核(Meteor Lake 等),
//!   传感器命名 "P-Core #n" / "E-Core #n"
//! - 功耗墙:RAPL PKG_POWER_LIMIT(0x610)读 PL1/PL2(驱动层),
//!   手动手持机(MSI Claw 等)的 TDP 档位切换直接反映在这里
//! - NPU:WMI PnP 枚举 "AI Boost"(Meteor Lake 起)
//! - 温度/功耗:驱动增强层(MSR/RAPL/SMN),不可用时标记 NO_DRIVER

#![cfg(target_os = "windows")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::options::UsageMode;
use crate::platform::windows::{driver, pdh, shared_wmi};
use crate::traits::Provider;
use std::sync::Mutex;
use std::time::Duration;
use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS,
};
use windows::Win32::System::Power::{
    CallNtPowerInformation, PROCESSOR_POWER_INFORMATION, ProcessorInformation,
};
use windows::Win32::System::Threading::GetSystemTimes;

crate::wmi_struct!(WmiProcessor, "Win32_Processor", {
    Name: String,
    Manufacturer: String,
    NumberOfCores: u32,
    NumberOfLogicalProcessors: u32,
    MaxClockSpeed: u32,
    L2CacheSize: u32,
    L3CacheSize: u32,
    SocketDesignation: String,
    ProcessorId: String,
});

crate::wmi_struct!(WmiThermalZone, "MSAcpi_ThermalZoneTemperature", {
    InstanceName: String,
    CurrentTemperature: u32,
});

crate::wmi_struct!(WmiPnpEntity, "Win32_PnPEntity", {
    Name: String,
});

/// CPUID leaf 0x1A(Core Type & Native Model ID Enumeration)核心类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreType {
    Performance,
    Efficient,
    Unknown,
}

/// 在指定逻辑核上执行 CPUID leaf 0x1A(需先绑核)。
fn core_type_on_cpu(logical: u32) -> CoreType {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    if logical >= 64 {
        return CoreType::Unknown;
    }
    unsafe {
        let prev = SetThreadAffinityMask(GetCurrentThread(), 1usize << logical);
        if prev == 0 {
            return CoreType::Unknown;
        }
        // CPUID leaf 0x1A EAX bits 31:24:0x20 = Atom(E 核),0x40 = Core(P 核)
        let r = std::arch::x86_64::__cpuid_count(0x1A, 0);
        SetThreadAffinityMask(GetCurrentThread(), prev);
        match r.eax >> 24 {
            0x40 => CoreType::Performance,
            0x20 => CoreType::Efficient,
            _ => CoreType::Unknown,
        }
    }
}

/// 逐核核心类型;不支持混合架构(leaf 0x1A 全 0)时返回 None。
fn detect_core_types(logical: usize) -> Option<Vec<CoreType>> {
    let types: Vec<CoreType> = (0..logical.min(64) as u32)
        .map(core_type_on_cpu)
        .collect();
    let any_known = types
        .iter()
        .any(|t| !matches!(t, CoreType::Unknown));
    // 单一类型 = 非混合架构,不必标注
    let mixed = types.iter().any(|t| *t == CoreType::Performance)
        && types.iter().any(|t| *t == CoreType::Efficient);
    (any_known && mixed).then_some(types)
}

/// PDH 逐核采样结果([`CpuProvider::sample_pdh`])。
enum PdhSample {
    /// PDH 不可用(构建失败):任务管理器口径占用报 Error,频率回退标称值
    Unavailable,
    /// 首轮/长时间间隔后重建基线:本轮无值,保持 Waiting 或旧值
    Baseline,
    /// 采集失败(此前有可用查询):任务管理器口径报 Error,频率保留旧值
    Failed,
    /// (逻辑处理器索引, utility %, performance %, actual frequency MHz)
    Values(Vec<(u32, f64, f64, f64)>),
}

/// PDH 逐核计数器组(查询建一次,实例静态):
/// - % Processor Utility:频率加权忙碌度(原始);
/// - % Processor Performance:实际频率 / 标称频率;
/// - Actual Frequency:实际频率(MHz,实时反映睿频/降频)。
/// 任务管理器(Win11 24H2+)默认占用 = Utility ÷ Performance(归一化,
/// 等价于时间占比);逐核频率两种口径都取 Actual Frequency。
struct UtilityPdh {
    query: pdh::PdhQuery,
    /// (逻辑处理器索引, utility, performance, actual-frequency)
    counters: Vec<(u32, pdh::PdhCounter, pdh::PdhCounter, pdh::PdhCounter)>,
    last_collect: Option<std::time::Instant>,
    warned_read: bool,
}

struct CpuState {
    /// 每逻辑核 (idle, kernel, user) 100ns 计数
    last_core_times: Option<Vec<(u64, u64, u64)>>,
    last_times: Option<(u64, u64)>,
    last_at: Option<std::time::Instant>,
    core_count: usize,
    /// probe 声明的 ACPI 热区温度传感器数(0 = 未声明)
    tz_count: usize,
    /// 热区 WMI 查询节流(查询较慢,5s 一次足够)
    tz_last: Option<std::time::Instant>,
    /// PDH 逐核计数器组(懒创建,常驻;占用+频率共用一次采集)
    utility: Option<UtilityPdh>,
    /// 上次查询构建尝试(失败后 5s 节流重试)
    utility_retry_at: Option<std::time::Instant>,
    utility_warned_open: bool,
}

pub struct CpuProvider {
    state: Mutex<CpuState>,
}

impl CpuProvider {
    pub fn new() -> CpuProvider {
        CpuProvider {
            state: Mutex::new(CpuState {
                last_core_times: None,
                last_times: None,
                last_at: None,
                core_count: 0,
                tz_count: 0,
                tz_last: None,
                utility: None,
                utility_retry_at: None,
                utility_warned_open: false,
            }),
        }
    }

    /// ACPI 热区温度(°C)。WMI MSAcpi_ThermalZoneTemperature,
    /// CurrentTemperature 单位 0.1K;无效读数(0 / 越界)跳过,最多 max 个。
    /// probe 与 refresh 共用,保证两边过滤规则一致。
    fn thermal_zone_temps(max: usize) -> Vec<f64> {
        let Ok(conn) = shared_wmi() else {
            return Vec::new();
        };
        let Ok(zones) = conn.raw_query::<WmiThermalZone>(
            "SELECT InstanceName, CurrentTemperature FROM MSAcpi_ThermalZoneTemperature",
        ) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for z in &zones {
            let Some(t) = z.CurrentTemperature else { continue };
            if t == 0 {
                continue;
            }
            let c = t as f64 / 10.0 - 273.15;
            if !(-40.0..150.0).contains(&c) {
                continue;
            }
            out.push((c * 10.0).round() / 10.0);
            if out.len() >= max {
                break;
            }
        }
        out
    }

    fn load_times() -> Option<(u64, u64)> {
        unsafe {
            let mut idle = Default::default();
            let mut kernel = Default::default();
            let mut user = Default::default();
            if !GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() {
                return None;
            }
            let ft = |f: &windows::Win32::Foundation::FILETIME| {
                (f.dwLowDateTime as u64) | ((f.dwHighDateTime as u64) << 32)
            };
            Some((ft(&idle), ft(&kernel).wrapping_add(ft(&user))))
        }
    }

    /// NtQuerySystemInformation(SystemProcessorPerformanceInformation):
    /// 每逻辑核 (idle, kernel, user),100ns 计数。kernel 含 idle。
    fn per_core_times() -> Option<Vec<(u64, u64, u64)>> {
        // windows-rs 未导出该结构,自行声明(5×i64 + u32)
        #[repr(C)]
        struct ProcPerfInfo {
            idle: i64,
            kernel: i64,
            user: i64,
            dpc: i64,
            interrupt: i64,
            interrupt_count: u32,
        }
        unsafe {
            let item = std::mem::size_of::<ProcPerfInfo>();
            // 1024 逻辑核上限:覆盖 Windows Server 多组场景(256 不够)
            let max_cores = 1024usize;
            let mut buf = vec![0u8; item * max_cores];
            let status = NtQuerySystemInformation(
                SYSTEM_INFORMATION_CLASS(8), // SystemProcessorPerformanceInformation
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                std::ptr::null_mut(),
            );
            if status.0 < 0 {
                return None;
            }
            // 返回长度由内核填充:按成功时 count = buffer/item,依赖 NTSTATUS;
            // NtQuery 不返回实际长度时按 12/256 核策略:用 core_count 校正由调用方处理,
            // 这里全部解析(空闲区域的 0 值不影响差分,因为差分前会按 core_count 截断)
            let count = buf.len() / item;
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let p = buf.as_ptr().add(i * item) as *const ProcPerfInfo;
                let info = &*p;
                out.push((info.idle as u64, info.kernel as u64, info.user as u64));
            }
            Some(out)
        }
    }

    fn per_core_frequencies() -> Vec<(u32, u32)> {
        // (core_index, current_mhz)
        let mut out = Vec::new();
        for size in [4096usize, 16384, 65536] {
            let mut buf = vec![0u8; size];
            let status = unsafe {
                CallNtPowerInformation(
                    ProcessorInformation,
                    None,
                    0,
                    Some(buf.as_mut_ptr() as *mut _),
                    buf.len() as u32,
                )
            };
            if !status.is_ok() {
                continue;
            }
            let item_size = std::mem::size_of::<PROCESSOR_POWER_INFORMATION>();
            let count = buf.len() / item_size;
            for i in 0..count {
                let p = unsafe { buf.as_ptr().add(i * item_size) } as *const PROCESSOR_POWER_INFORMATION;
                let info = unsafe { &*p };
                out.push((info.Number, info.CurrentMhz));
            }
            break;
        }
        out
    }

    fn open_utility_query() -> Result<UtilityPdh, String> {
        let mut query = pdh::PdhQuery::new()?;
        // PdhAddCounter 不接受通配符,先展开为具体实例再逐个添加;
        // 三条计数器路径按逻辑处理器索引配对
        let mut utils = std::collections::HashMap::new();
        for p in pdh::expand_english("Processor Information(*)\\% Processor Utility")? {
            if let Some(lp) = pdh::path_instance(&p).and_then(Self::lp_index_from_instance) {
                utils.insert(lp, query.add(&p)?);
            }
        }
        let mut perfs = std::collections::HashMap::new();
        for p in pdh::expand_english("Processor Information(*)\\% Processor Performance")? {
            if let Some(lp) = pdh::path_instance(&p).and_then(Self::lp_index_from_instance) {
                perfs.insert(lp, query.add(&p)?);
            }
        }
        let mut freqs = std::collections::HashMap::new();
        for p in pdh::expand_english("Processor Information(*)\\Actual Frequency")? {
            if let Some(lp) = pdh::path_instance(&p).and_then(Self::lp_index_from_instance) {
                freqs.insert(lp, query.add(&p)?);
            }
        }
        let mut counters: Vec<_> = utils
            .into_iter()
            .filter_map(|(lp, u)| {
                let p = perfs.remove(&lp)?;
                let f = freqs.remove(&lp)?;
                Some((lp, u, p, f))
            })
            .collect();
        counters.sort_by_key(|(lp, ..)| *lp);
        if counters.is_empty() {
            return Err("Processor Information 展开后无有效实例".into());
        }
        Ok(UtilityPdh {
            query,
            counters,
            last_collect: None,
            warned_read: false,
        })
    }

    /// PDH 实例名 → 逻辑处理器索引:"0,3" → 3、"0,0,66" → 66(取最后一段);
    /// 任一段非数字(如 _Total 及其本地化变体)返回 None。
    fn lp_index_from_instance(name: &str) -> Option<u32> {
        let mut last = None;
        for part in name.trim().split(',') {
            last = Some(part.trim().parse::<u32>().ok()?);
        }
        last
    }

    /// PDH 逐核采样(占用 + 频率共用一次采集)。懒构建,失败 5s 节流重试。
    fn sample_pdh(&self, now: std::time::Instant) -> PdhSample {
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if st.utility.is_none()
                && st
                    .utility_retry_at
                    .map(|t| now.duration_since(t) >= Duration::from_secs(5))
                    .unwrap_or(true)
            {
                st.utility_retry_at = Some(now);
                match Self::open_utility_query() {
                    Ok(u) => st.utility = Some(u),
                    Err(e) => {
                        if !st.utility_warned_open {
                            st.utility_warned_open = true;
                            crate::logbuf::warn(format!(
                                "CPU: PDH 逐核计数器不可用,任务管理器口径占用标记为 ERROR、频率回退标称值(每 5 秒重试): {e}"
                            ));
                        }
                    }
                }
            }
            if st.utility.is_none() {
                return PdhSample::Unavailable;
            }
        }

        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(u) = st.utility.as_mut() else {
            return PdhSample::Unavailable;
        };
        // 休眠恢复/长时间未采集:差分跨度过大,本轮只重建基线
        let gap_rebuild = u
            .last_collect
            .map(|t| now.duration_since(t) > Duration::from_secs(10))
            .unwrap_or(false);
        let mut rows = Vec::with_capacity(u.counters.len());
        let mut read_err = None;
        if let Err(e) = u.query.collect() {
            read_err = Some(e);
        } else {
            for (lp, cu, cp, cf) in &u.counters {
                let (Ok(util), Ok(perf), Ok(freq)) = (cu.value_f64(), cp.value_f64(), cf.value_f64())
                else {
                    continue;
                };
                if util.is_finite() && perf.is_finite() && freq.is_finite() {
                    rows.push((*lp, util, perf, freq));
                }
            }
        }
        u.last_collect = Some(now);
        if let Some(e) = read_err {
            if !u.warned_read {
                u.warned_read = true;
                crate::logbuf::warn(format!(
                    "CPU: PDH 逐核计数器采集失败(恢复后自动更新): {e}"
                ));
            }
            return PdhSample::Failed;
        }
        if rows.is_empty() || gap_rebuild {
            return PdhSample::Baseline; // 首轮/重建基线:保持 Waiting 或旧值
        }
        PdhSample::Values(rows)
    }
}

impl Provider for CpuProvider {
    fn describe(&self) -> &str {
        "win-cpu"
    }

    fn category(&self) -> Category {
        Category::Cpu
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        // CPUID 基础信息
        let cpuid = raw_cpuid::CpuId::new();
        let cpuid_vendor = cpuid
            .get_vendor_info()
            .map(|v| v.as_str().to_string())
            .unwrap_or_default();
        let cpuid_brand = cpuid
            .get_processor_brand_string()
            .map(|b| b.as_str().to_string())
            .unwrap_or_default();

        // WMI 详细信息(失败则回退 CPUID)
        let (mut name, mut vendor, mut wmi_cores, mut wmi_logical, mut max_mhz, mut l2, mut l3, mut socket, mut proc_id) =
            (
                cpuid_brand.clone(),
                cpuid_vendor.clone(),
                0u32,
                0u32,
                0u32,
                0u32,
                0u32,
                String::new(),
                String::new(),
            );
        match shared_wmi() {
            Ok(conn) => {
                let rows = conn
                    .raw_query::<WmiProcessor>(
                        "SELECT Name, Manufacturer, NumberOfCores, NumberOfLogicalProcessors, \
                         MaxClockSpeed, L2CacheSize, L3CacheSize, SocketDesignation, ProcessorId \
                         FROM Win32_Processor",
                    )
                    .map_err(|e| format!("WMI Win32_Processor 查询失败: {e}"))?;
                if let Some(p) = rows.first() {
                    if let Some(v) = &p.Name {
                        if !v.is_empty() {
                            name = v.trim().to_string();
                        }
                    }
                    if let Some(v) = &p.Manufacturer {
                        vendor = v.clone();
                    }
                    wmi_cores = p.NumberOfCores.unwrap_or(0);
                    wmi_logical = p.NumberOfLogicalProcessors.unwrap_or(0);
                    max_mhz = p.MaxClockSpeed.unwrap_or(0);
                    l2 = p.L2CacheSize.unwrap_or(0);
                    l3 = p.L3CacheSize.unwrap_or(0);
                    socket = p.SocketDesignation.clone().unwrap_or_default();
                    proc_id = p.ProcessorId.clone().unwrap_or_default();
                }
            }
            Err(e) => {
                crate::logbuf::warn(format!("CPU provider: WMI 不可用,回退 CPUID: {e}"));
            }
        }
        if wmi_cores == 0 {
            wmi_cores = cpuid
                .get_feature_info()
                .map(|f| f.max_logical_processor_ids() as u32)
                .unwrap_or(0);
        }
        if wmi_logical == 0 {
            wmi_logical = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(0);
        }
        let core_count = wmi_logical.max(1) as usize;
        self.state.lock().unwrap_or_else(|e| e.into_inner()).core_count = core_count;

        let mut b = DeviceBuilder::new("cpu.package.0", name.clone())
            .vendor(vendor)
            .model(name.clone())
            .serial(if proc_id.is_empty() { format!("cpu:{cpuid_brand}") } else { proc_id })
            .static_str("brand", "Brand String", &cpuid_brand)
            .static_f64("cores", "Physical Cores", "", wmi_cores as f64)
            .static_f64("logical", "Logical Processors", "", wmi_logical as f64);

        // Intel 代际识别:CPUID 签名 → 平台代号 / 微架构 / 市场代际 / 制程
        if cpuid_vendor.eq_ignore_ascii_case("GenuineIntel") {
            if let Some(fi) = cpuid.get_feature_info() {
                let (f, m, s) = (fi.family_id(), fi.model_id(), fi.stepping_id());
                b = b.static_str(
                    "cpu_signature",
                    "CPUID Signature",
                    format!("Family {f} Model {m:X} Stepping {s}"),
                );
                if let Some(info) = crate::intel_id::cpu_info(f, m, s) {
                    b = b.static_str("codename", "Codename", info.codename);
                    if !info.microarch.is_empty() {
                        b = b.static_str("microarch", "Microarchitecture", info.microarch);
                    }
                    if !info.generation.is_empty() {
                        b = b.static_str("generation", "Generation", info.generation);
                    }
                    if !info.process.is_empty() {
                        b = b.static_str("process_node", "Process Node", info.process);
                    }
                }
            }
        }

        if max_mhz > 0 {
            b = b.static_f64("max_clock_mhz", "Max Clock", "MHz", max_mhz as f64);
        }
        if l2 > 0 {
            b = b.static_f64("l2_cache_kb", "L2 Cache", "KB", l2 as f64);
        }
        if l3 > 0 {
            b = b.static_f64("l3_cache_kb", "L3 Cache", "KB", l3 as f64);
        }
        if !socket.is_empty() {
            b = b.static_str("socket", "Socket", socket);
        }

        // 混合架构:逐核 CPUID leaf 0x1A 区分 P/E 核(Meteor Lake 及以后)
        let core_types = if cpuid_vendor.eq_ignore_ascii_case("GenuineIntel") {
            detect_core_types(core_count)
        } else {
            None
        };
        if let Some(types) = &core_types {
            let p_threads = types.iter().filter(|t| **t == CoreType::Performance).count();
            let e_threads = types.iter().filter(|t| **t == CoreType::Efficient).count();
            // 物理核数 = P 核 + E 核(E 核单线程)。优先用 WMI NumberOfCores
            // 反推 P 核数(固件关闭 HT 时按 p_threads/2 会算错一半);
            // WMI 值缺失时退回假设 HT 开启。LP-E 核无法与 E 核区分,并入 E 计。
            let wmi_cores_us = wmi_cores as usize;
            let p_cores = if wmi_cores_us >= e_threads {
                wmi_cores_us - e_threads
            } else {
                p_threads / 2
            };
            let ht = p_threads > p_cores;
            b = b.static_str(
                "core_topology",
                "Core Topology",
                format!(
                    "{p_cores} P-Cores{} + {e_threads} E-Cores (incl. LP-E)",
                    if ht { " (HT)" } else { "" }
                ),
            );
        }

        // NPU(Meteor Lake 起集成 AI Boost):WMI PnP 枚举,失败静默
        if cpuid_vendor.eq_ignore_ascii_case("GenuineIntel") {
            if let Ok(conn) = shared_wmi() {
                if let Ok(rows) = conn.raw_query::<WmiPnpEntity>(
                    "SELECT Name FROM Win32_PnPEntity WHERE Name LIKE '%AI Boost%'",
                ) {
                    if let Some(n) = rows.first().and_then(|r| r.Name.clone()) {
                        if !n.trim().is_empty() {
                            b = b.static_str("npu", "Neural Processing Unit", n.trim());
                        }
                    }
                }
            }
        }

        // 逐核显示名:P/E 核各自编号(P-Core #0… / E-Core #0…)
        let core_labels: Vec<String> = {
            let mut p = 0usize;
            let mut e = 0usize;
            let mut u = 0usize;
            (0..core_count)
                .map(|i| match core_types.as_ref().and_then(|ts| ts.get(i)) {
                    Some(CoreType::Performance) => {
                        let l = format!("P-Core #{p}");
                        p += 1;
                        l
                    }
                    Some(CoreType::Efficient) => {
                        let l = format!("E-Core #{e}");
                        e += 1;
                        l
                    }
                    _ => {
                        let l = format!("Core #{u}");
                        u += 1;
                        l
                    }
                })
                .collect()
        };

        // 动态传感器:总占用 + 逐核占用 + 逐核频率
        b = b.dynamic("load_total", "CPU Usage", "%", ValueType::F64);
        for (i, label) in core_labels.iter().enumerate() {
            b = b.dynamic(
                &format!("load_core_{i}"),
                &format!("{label} Usage"),
                "%",
                ValueType::F64,
            );
            b = b.dynamic(
                &format!("freq_core_{i}"),
                &format!("{label} Clock"),
                "MHz",
                ValueType::F64,
            );
        }

        // 驱动增强:温度/功耗(可用时才有传感器,键保持稳定)
        let drv = driver::is_available();
        if drv {
            if cpuid_vendor.eq_ignore_ascii_case("GenuineIntel") {
                b = b.dynamic("temperature_pkg", "CPU Package Temperature", "°C", ValueType::F64);
                for (i, label) in core_labels.iter().enumerate() {
                    b = b.dynamic(
                        &format!("temperature_core_{i}"),
                        &format!("{label} Temperature"),
                        "°C",
                        ValueType::F64,
                    );
                }
                b = b.dynamic("power_pkg", "CPU Package Power", "W", ValueType::F64);
                // RAPL 功耗墙(手持机 TDP 档位切换直接反映在这两个字段);
                // PL2 为 0 的平台(未实现该域)不声明 PL2
                if let Some((pl1, pl2)) = driver::read_package_power_limits() {
                    b = b.dynamic(
                        "power_limit_pl1_w",
                        "Power Limit PL1 (Sustained)",
                        "W",
                        ValueType::F64,
                    );
                    if pl2.is_some() {
                        b = b.dynamic(
                            "power_limit_pl2_w",
                            "Power Limit PL2 (Turbo)",
                            "W",
                            ValueType::F64,
                        );
                    }
                }
            } else if cpuid_vendor.eq_ignore_ascii_case("AuthenticAMD") {
                b = b.dynamic("temperature_tctl", "CPU Tctl Temperature", "°C", ValueType::F64);
                b = b.dynamic("power_pkg", "CPU Package Power", "W", ValueType::F64);
            }
        }

        // 用户态温度回退:ACPI 热区(HVCI 等场景下驱动不可用时的兜底)。
        // 首值由 probe 写入,refresh 按 5s 节流持续更新(见 refresh)。
        if !drv {
            let tz_temps = Self::thermal_zone_temps(4);
            for (tz_idx, c) in tz_temps.iter().enumerate() {
                b = b.dynamic_now(
                    &format!("temperature_tz_{tz_idx}"),
                    &format!("Thermal Zone #{tz_idx}"),
                    "°C",
                    Value::F64(*c),
                );
            }
            if tz_temps.is_empty() {
                crate::logbuf::info("CPU: 使用 ACPI 热区温度(0 个)作为用户态回退");
            } else {
                crate::logbuf::info(format!(
                    "CPU: 使用 ACPI 热区温度({} 个)作为用户态回退",
                    tz_temps.len()
                ));
                self.state.lock().unwrap_or_else(|e| e.into_inner()).tz_count = tz_temps.len();
            }
        }

        // 首轮采样在 probe 不做,refresh 首轮只记录基线
        Ok(vec![b.build()])
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let dev = match devices.first_mut() {
            Some(d) => d,
            None => return,
        };

        // 采样:系统级 + 逐核,并与上一轮做差分
        let core_times = Self::per_core_times();
        let sys_times = Self::load_times();
        let now = std::time::Instant::now();
        let (prev_core, prev_total, dt) = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let pc = st.last_core_times.replace(core_times.clone().unwrap_or_default());
            let pt = st.last_times.replace(sys_times.unwrap_or((0, 0)));
            let d = st
                .last_at
                .replace(now)
                .map(|t| now.duration_since(t).as_secs_f64());
            (pc, pt, d)
        };
        let core_count = { self.state.lock().unwrap_or_else(|e| e.into_inner()).core_count };

        // 占用与频率:按全局口径分支。时间差分采样始终进行(上方),保证
        // 两种口径来回切换时基线一直新鲜,切换即时生效无需重建。
        //
        // PDH 逐核计数器两种口径都要用:任务管理器口径的占用
        // (Utility÷Performance 归一化,同 Win11 24H2+ 任务管理器默认视图)
        // 与两种口径共用的逐核实时频率(Actual Frequency)。
        let tm = crate::options::usage_mode() == UsageMode::TaskManager;
        match self.sample_pdh(now) {
            PdhSample::Values(rows) => {
                // 逐核频率:两种口径一致
                let mut sum_util = 0.0;
                let mut sum_perf = 0.0;
                for (lp, util, perf, freq) in &rows {
                    if (*lp as usize) < core_count {
                        dev.set_reading(
                            &format!("freq_core_{lp}"),
                            Reading::now(Value::F64(freq.round())),
                        );
                    }
                    sum_util += util;
                    sum_perf += perf;
                }
                // 任务管理器口径占用:Utility÷Performance 归一化(封顶 100%),
                // 即频率无关的"忙碌时间占比",同任务管理器 24H2+ 默认显示
                if tm {
                    if sum_perf > 0.0 {
                        dev.set_reading(
                            "load_total",
                            Reading::now(Value::F64(
                                ((sum_util / sum_perf * 100.0).clamp(0.0, 100.0) * 10.0).round()
                                    / 10.0,
                            )),
                        );
                    }
                    for (lp, util, perf, _) in &rows {
                        if (*lp as usize) < core_count && *perf > 0.5 {
                            dev.set_reading(
                                &format!("load_core_{lp}"),
                                Reading::now(Value::F64(
                                    ((util / perf * 100.0).clamp(0.0, 100.0) * 10.0).round() / 10.0,
                                )),
                            );
                        }
                    }
                }
            }
            PdhSample::Unavailable => {
                // 频率回退:CallNtPowerInformation(标称频率,不反映睿频/降频)
                for (num, mhz) in Self::per_core_frequencies() {
                    let idx = num as usize;
                    if idx < core_count {
                        dev.set_reading(
                            &format!("freq_core_{idx}"),
                            Reading::now(Value::F64(mhz as f64)),
                        );
                    }
                }
                if tm {
                    dev.set_reading("load_total", Reading::failed(Status::Error, ValueType::F64));
                    for i in 0..core_count {
                        dev.set_reading(
                            &format!("load_core_{i}"),
                            Reading::failed(Status::Error, ValueType::F64),
                        );
                    }
                }
            }
            PdhSample::Failed => {
                if tm {
                    dev.set_reading("load_total", Reading::failed(Status::Error, ValueType::F64));
                    for i in 0..core_count {
                        dev.set_reading(
                            &format!("load_core_{i}"),
                            Reading::failed(Status::Error, ValueType::F64),
                        );
                    }
                }
                // 频率:保留旧值(标称常量回退会制造假的稳定读数)
            }
            PdhSample::Baseline => {} // 首轮/重建基线:保持 Waiting 或旧值
        }

        if !tm {
            // 总占用:GetSystemTimes 差分
            match (prev_total, sys_times, dt) {
                (Some((pidle, ptotal)), Some((idle, total)), Some(d))
                    if d > 0.05 && total > ptotal =>
                {
                    let load = (1.0 - (idle.saturating_sub(pidle)) as f64 / (total - ptotal) as f64)
                        * 100.0;
                    dev.set_reading(
                        "load_total",
                        Reading::now(Value::F64(load.clamp(0.0, 100.0))),
                    );
                }
                (None, _, _) if core_times.is_some() => {
                    dev.set_reading("load_total", Reading::waiting(ValueType::F64));
                }
                (Some(_), Some(_), _) => {
                    dev.set_reading("load_total", Reading::waiting(ValueType::F64));
                }
                _ => {
                    dev.set_reading("load_total", Reading::failed(Status::Error, ValueType::F64));
                }
            }

            // 逐核占用:NtQuerySystemInformation 差分。kernel 时间含 idle,
            // busy = (kernel - idle) + user,分母 = kernel + user
            match (prev_core, &core_times) {
                (Some(prev), Some(cur)) => {
                    for i in 0..core_count.min(prev.len()).min(cur.len()) {
                        let (pidle, pk, pu) = prev[i];
                        let (idle, k, u) = cur[i];
                        let d_idle = idle.saturating_sub(pidle) as f64;
                        let d_kernel = k.saturating_sub(pk) as f64;
                        let d_user = u.saturating_sub(pu) as f64;
                        let d_all = d_kernel + d_user;
                        if d_all > 0.0 {
                            let load =
                                ((d_kernel - d_idle + d_user) / d_all * 100.0).clamp(0.0, 100.0);
                            dev.set_reading(
                                &format!("load_core_{i}"),
                                Reading::now(Value::F64((load * 10.0).round() / 10.0)),
                            );
                        }
                    }
                }
                (_, None) => {
                    for i in 0..core_count {
                        dev.set_reading(
                            &format!("load_core_{i}"),
                            Reading::failed(Status::Error, ValueType::F64),
                        );
                    }
                }
                (None, Some(_)) => {} // 首轮只记录基线
            }
        }

        // 驱动增强:温度/功耗
        if driver::is_available() {
            driver::refresh_cpu_enhanced(dev, core_count);
            if let Some((pl1, pl2)) = driver::read_package_power_limits() {
                dev.set_reading("power_limit_pl1_w", Reading::now(Value::F64(pl1)));
                if let Some(pl2) = pl2 {
                    dev.set_reading("power_limit_pl2_w", Reading::now(Value::F64(pl2)));
                }
            }
        }

        // 热区温度回退:WMI 查询较慢,5s 节流;查询失败保留旧值
        let tz_count = { self.state.lock().unwrap_or_else(|e| e.into_inner()).tz_count };
        if tz_count > 0 {
            let due = {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let due = st
                    .tz_last
                    .map(|t| t.elapsed() >= Duration::from_secs(5))
                    .unwrap_or(true);
                if due {
                    st.tz_last = Some(now);
                }
                due
            };
            if due {
                for (i, c) in Self::thermal_zone_temps(4)
                    .into_iter()
                    .take(tz_count)
                    .enumerate()
                {
                    dev.set_reading(
                        &format!("temperature_tz_{i}"),
                        Reading::now(Value::F64(c)),
                    );
                }
            }
        }
    }
}
