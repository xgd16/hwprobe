//! GPU provider:DXGI 枚举 + PDH GPU Engine 利用率 + DXGI 显存 + NVML(NVIDIA)
//! + ADLX(AMD)。
//!
//! 通用路径(DXGI/PDH)覆盖任意厂商;NVIDIA 叠加 NVML 完整传感器
//! (温度/功耗/风扇/频率),AMD 叠加 ADLX(温度/热点/功耗/风扇/显存/频率),
//! 均运行时加载,缺失自动降级。
//!
//! 利用率:分引擎占用(3D/Copy/VideoDecode/VideoEncode/Compute)始终来自
//! GPU Engine 计数器;util_gpu_pct 总占用由 `options::set_usage_mode` 切换 ——
//! 标准口径取厂商 API,任务管理器口径取 GPU Engine 按引擎类型聚合的最忙值。

#![cfg(target_os = "windows")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::options::UsageMode;
use crate::platform::windows::{driver, gpu_adlx, gpu_nvml, pdh, shared_wmi};
use crate::traits::Provider;
use std::sync::Mutex;
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIAdapter1, IDXGIAdapter3, DXGI_ADAPTER_DESC1,
    DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
};

crate::wmi_struct!(WmiVideoController, "Win32_VideoController", {
    Name: String,
    DriverVersion: String,
    AdapterCompatibility: String,
    PNPDeviceID: String,
});

/// 分引擎占用传感器:(传感器编码, 显示名, GPU Engine 实例名 engtype 令牌)。
/// 数据源为 PDH GPU Engine 计数器(任务管理器同源),两种口径都提供;
/// 冷门引擎类型(Crypto/Overlay/VideoProcessing 等)无独立传感器,
/// 但计入任务管理器口径 util_gpu_pct 的"最忙引擎"聚合。
const ENGINE_SENSORS: [(&str, &str, &str); 5] = [
    ("util_eng_3d", "3D Engine Usage", "3D"),
    ("util_eng_copy", "Copy Engine Usage", "Copy"),
    ("util_eng_video_decode", "Video Decode Engine Usage", "VideoDecode"),
    ("util_eng_video_encode", "Video Encode Engine Usage", "VideoEncode"),
    ("util_eng_compute", "Compute Engine Usage", "Compute"),
];

/// GPU Engine 利用率计数器。实例随进程动态增减(pid_xxx),查询定期重建;
/// 展开时即按实例名解析 LUID 与 engtype,读取时逐计数器取值。
struct EnginePdh {
    query: pdh::PdhQuery,
    /// ((LUID HighPart, LUID LowPart), engtype, 计数器)
    counters: Vec<((u32, u32), String, pdh::PdhCounter)>,
    built_at: std::time::Instant,
    last_collect: Option<std::time::Instant>,
    warned_read: bool,
}

struct GpuState {
    /// DXGI adapter COM 对象按设备顺序存储(refresh 时查显存)
    adapters: Vec<Option<IDXGIAdapter3>>,
    /// 设备序号 → ADLX GPU 序号(AMD 且 ADLX 可用时)
    adlx_map: Vec<Option<usize>>,
    /// 设备序号 → 是否挂接 IGCL(Intel 核显且配对成功)
    intel_igcl: Vec<bool>,
    /// 设备序号 → 适配器 LUID(HighPart, LowPart),GPU Engine 实例配对用
    luids: Vec<(u32, u32)>,
    /// GPU Engine 计数器查询(懒创建,每 10s 重建以跟踪新实例)
    engines: Option<EnginePdh>,
    /// 上次查询构建尝试(首次/失败后 5s 节流重试)
    engines_retry_at: Option<std::time::Instant>,
    engines_warned_open: bool,
}

pub struct GpuProvider {
    state: Mutex<GpuState>,
}

impl GpuProvider {
    pub fn new() -> GpuProvider {
        GpuProvider {
            state: Mutex::new(GpuState {
                adapters: Vec::new(),
                adlx_map: Vec::new(),
                intel_igcl: Vec::new(),
                luids: Vec::new(),
                engines: None,
                engines_retry_at: None,
                engines_warned_open: false,
            }),
        }
    }

    fn vendor_name(vendor_id: u32) -> &'static str {
        match vendor_id {
            0x10DE => "NVIDIA",
            0x1002 | 0x1022 => "AMD",
            0x8086 => "Intel",
            _ => "",
        }
    }

    /// 展开通配符并逐实例添加计数器(PdhAddCounter 不接受通配符)。
    fn build_engine_query() -> Result<EnginePdh, String> {
        let mut query = pdh::PdhQuery::new()?;
        let paths = pdh::expand_english("GPU Engine(*)\\Utilization Percentage")?;
        let mut counters = Vec::with_capacity(paths.len());
        for p in paths {
            let Some(inst) = pdh::path_instance(&p) else {
                continue;
            };
            let Some((high, low, engtype)) = Self::parse_engine_instance(inst) else {
                continue;
            };
            counters.push(((high, low), engtype.to_string(), query.add(&p)?));
        }
        Ok(EnginePdh {
            query,
            counters,
            built_at: std::time::Instant::now(),
            last_collect: None,
            warned_read: false,
        })
    }

    /// GPU Engine 实例名 → (LUID HighPart, LUID LowPart, engtype)。
    /// 形如 "pid_1234_luid_0x00000000_0x0000E3B1_phys_0_eng_0_engtype_3D"
    /// (部分构建无 phys 段,均兼容)。
    fn parse_engine_instance(name: &str) -> Option<(u32, u32, &str)> {
        let rest = &name[name.find("luid_0x")? + "luid_0x".len()..];
        let mut hex = rest.split('_');
        let high = u32::from_str_radix(hex.next()?, 16).ok()?;
        let low = u32::from_str_radix(hex.next()?.strip_prefix("0x")?, 16).ok()?;
        let engtype = name.split("engtype_").nth(1)?;
        Some((high, low, engtype))
    }

    /// 分引擎占用:GPU Engine 计数器按 LUID × engtype 聚合。两种口径都写
    /// 5 个分引擎传感器;任务管理器口径下另以"最忙引擎类型"覆盖 util_gpu_pct。
    /// 实例随进程动态增减,查询每 10s 重建(新进程最迟 10s 后纳入统计)。
    fn refresh_engines(&self, devices: &mut [DeviceData]) {
        if devices.is_empty() {
            return;
        }
        let fail_all = |devices: &mut [DeviceData]| {
            for dev in devices.iter_mut() {
                for (code, _, _) in ENGINE_SENSORS {
                    dev.set_reading(code, Reading::failed(Status::Error, ValueType::F64));
                }
                if crate::options::usage_mode() == UsageMode::TaskManager {
                    dev.set_reading("util_gpu_pct", Reading::failed(Status::Error, ValueType::F64));
                }
            }
        };

        let now = std::time::Instant::now();
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let build_due = match st.engines.as_ref() {
                Some(e) => e.built_at.elapsed() >= Duration::from_secs(10),
                None => st
                    .engines_retry_at
                    .map(|t| now.duration_since(t) >= Duration::from_secs(5))
                    .unwrap_or(true),
            };
            if build_due {
                st.engines_retry_at = Some(now);
                match Self::build_engine_query() {
                    Ok(e) => st.engines = Some(e),
                    Err(err) => {
                        // 已有旧查询时重建失败:保留旧查询继续用(实例可能过期但仍有值)
                        if st.engines.is_none() && !st.engines_warned_open {
                            st.engines_warned_open = true;
                            crate::logbuf::warn(format!(
                                "GPU: PDH GPU Engine 计数器不可用,分引擎占用标记为 ERROR(将自动重试): {err}"
                            ));
                        }
                    }
                }
            }
            if st.engines.is_none() {
                drop(st);
                fail_all(devices);
                return;
            }
        }

        let luids = {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .luids
                .clone()
        };

        // 采集 + 逐计数器读值(锁内完成,PDH 调用微秒级)
        enum Sample {
            Baseline,
            Values(Vec<((u32, u32), String, f64)>),
            Failed,
        }
        let sampled = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(e) = st.engines.as_mut() else {
                fail_all(devices);
                return;
            };
            // 休眠恢复/长时间未采集:差分跨度过大,本轮只重建基线
            let gap_rebuild = e
                .last_collect
                .map(|t| now.duration_since(t) > Duration::from_secs(10))
                .unwrap_or(false);
            let mut vals = Vec::with_capacity(e.counters.len());
            let mut read_err = None;
            if let Err(err) = e.query.collect() {
                read_err = Some(err);
            } else {
                for (luid, engtype, c) in &e.counters {
                    if let Ok(v) = c.value_f64() {
                        if v.is_finite() {
                            vals.push((*luid, engtype.clone(), v));
                        }
                    }
                }
            }
            e.last_collect = Some(now);
            if let Some(err) = read_err {
                if !e.warned_read {
                    e.warned_read = true;
                    crate::logbuf::warn(format!(
                        "GPU: GPU Engine 计数器采集失败(恢复后自动更新): {err}"
                    ));
                }
                Sample::Failed
            } else if vals.is_empty() || gap_rebuild {
                Sample::Baseline // 首轮/重建基线:保持 Waiting 或旧值
            } else {
                Sample::Values(vals)
            }
        };

        match sampled {
            Sample::Baseline => {} // 首轮/重建基线:保持 Waiting 或旧值
            Sample::Failed => fail_all(devices),
            Sample::Values(pairs) => {
                // 设备序号 → engtype → 利用率合计(同类型多引擎节点相加)
                let mut agg: Vec<std::collections::HashMap<String, f64>> =
                    (0..devices.len()).map(|_| std::collections::HashMap::new()).collect();
                for (luid, engtype, v) in pairs {
                    if let Some(di) = luids.iter().position(|l| *l == luid) {
                        *agg[di].entry(engtype).or_insert(0.0) += v;
                    }
                }
                let tm = crate::options::usage_mode() == UsageMode::TaskManager;
                for (di, dev) in devices.iter_mut().enumerate() {
                    let by_type = &agg[di];
                    for (code, _, ty) in ENGINE_SENSORS {
                        if let Some(v) = by_type.get(ty) {
                            dev.set_reading(
                                code,
                                Reading::now(Value::F64(
                                    (v.clamp(0.0, 100.0) * 10.0).round() / 10.0,
                                )),
                            );
                        }
                    }
                    if tm && !by_type.is_empty() {
                        let busiest = by_type.values().copied().fold(f64::MIN, f64::max);
                        dev.set_reading(
                            "util_gpu_pct",
                            Reading::now(Value::F64(busiest.clamp(0.0, 100.0))),
                        );
                    }
                }
            }
        }
    }
}

impl Provider for GpuProvider {
    fn describe(&self) -> &str {
        "win-gpu"
    }

    fn category(&self) -> Category {
        Category::Gpu
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        // 先建立 MTA COM 环境(WMI 首次初始化),DXGI 在 MTA 下创建的对象可跨线程使用
        let _ = shared_wmi();

        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| format!("CreateDXGIFactory1 失败: {e}"))?;

            // WMI 驱动版本(按名称模糊匹配,best-effort)
            let wmi_gpus = shared_wmi()
                .and_then(|conn| {
                    conn.raw_query::<WmiVideoController>(
                        "SELECT Name, DriverVersion, AdapterCompatibility, PNPDeviceID FROM Win32_VideoController",
                    )
                    .map_err(|e| e.to_string())
                })
                .unwrap_or_default();

            let mut devices = Vec::new();
            let mut adapters3 = Vec::new();
            let mut adlx_map: Vec<Option<usize>> = Vec::new();
            let mut device_vendor: Vec<u32> = Vec::new();
            let mut intel_igcl: Vec<bool> = Vec::new();
            let mut luids: Vec<(u32, u32)> = Vec::new();

            // 第一遍:枚举适配器(软件渲染器保留 None 占位,维持 adapters3 对齐语义)
            let mut raw: Vec<Option<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)>> = Vec::new();
            let mut i = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                match adapter.GetDesc1() {
                    Ok(desc) if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 => {
                        raw.push(Some((adapter, desc)))
                    }
                    _ => raw.push(None),
                }
            }

            // ADLX:存在 AMD 物理适配器时惰性初始化(运行库缺失/驱动不支持则静默降级)
            let any_amd = raw.iter().flatten().any(|(_, d)| d.VendorId == 0x1002);
            let adlx_gpus = if any_amd && gpu_adlx::ensure_init() {
                gpu_adlx::gpu_list()
            } else {
                Vec::new()
            };

            // IGCL:Intel 核显传感器(温度/功耗/频率/占用;Claw 等平台的唯一来源)
            let any_intel = raw.iter().flatten().any(|(_, d)| d.VendorId == 0x8086);
            let igcl_caps = if any_intel && super::gpu_igcl::is_integrated_available() {
                Some(super::gpu_igcl::caps())
            } else {
                None
            };
            let igcl_dids = super::gpu_igcl::integrated_pci_device_ids();

            for item in raw {
                let Some((adapter, desc)) = item else {
                    adapters3.push(None);
                    continue;
                };
                let name = String::from_utf16_lossy(
                    &desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(0)],
                );
                let vendor_id = desc.VendorId;
                let vendor = Self::vendor_name(vendor_id).to_string();
                let luid = format!(
                    "gpu.luid.{:08x}-{:08x}",
                    desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart
                );
                let vram_total_mb = desc.DedicatedVideoMemory / 1024 / 1024;

                let mut b = DeviceBuilder::new(luid.clone(), name.clone())
                    .vendor(if vendor.is_empty() { format!("VID {vendor_id:04X}") } else { vendor.clone() })
                    .model(name.clone())
                    .static_str("device_id", "PCI Device ID", format!("{:04X}:{:04X}", vendor_id, desc.DeviceId))
                    .static_f64("vram_total_mb", "VRAM Total", "MB", vram_total_mb as f64);

                // Intel 平台识别:PCI Device ID → 核显/独显平台代号 / IP 架构 / 系列
                if vendor_id == 0x8086 {
                    if let Some(g) = crate::intel_id::gpu_info(desc.DeviceId as u16) {
                        b = b.static_str("gpu_platform", "GPU Platform", g.platform);
                        if !g.architecture.is_empty() {
                            b = b.static_str("gpu_architecture", "GPU Architecture", g.architecture);
                        }
                        if !g.series.is_empty() {
                            b = b.static_str("gpu_series", "Graphics Series", g.series);
                        }
                        if let Some(eu) = g.execution_units {
                            b = b.static_f64("execution_units", "Execution Units", "", eu as f64);
                        }
                    }
                }

                // WMI 驱动版本:名称包含匹配;PNP 同时用于 ADLX 配对
                let mut adlx_idx: Option<usize> = None;
                if let Some(w) = wmi_gpus
                    .iter()
                    .find(|w| w.Name.as_deref().map(|n| name.contains(n.trim()) || n.contains(&name)).unwrap_or(false))
                {
                    if let Some(v) = &w.DriverVersion {
                        if !v.is_empty() {
                            b = b.static_str("driver_version", "Driver Version", v.clone());
                        }
                    }
                    if let Some(pnp) = &w.PNPDeviceID {
                        if !pnp.is_empty() {
                            b = b.static_str("pnp_id", "PNP Device ID", pnp.clone());
                            if vendor_id == 0x1002 && !adlx_gpus.is_empty() {
                                let key = gpu_adlx::pnp_key(pnp);
                                adlx_idx = adlx_gpus
                                    .iter()
                                    .find(|(_, k, _)| *k == key)
                                    .map(|(_, _, i)| *i);
                            }
                        }
                    }
                }

                // 动态传感器
                b = b.dynamic("util_gpu_pct", "GPU Usage", "%", ValueType::F64)
                    .dynamic("mem_used_mb", "VRAM Used", "MB", ValueType::F64)
                    .dynamic("mem_used_pct", "VRAM Usage", "%", ValueType::F64);
                // 分引擎占用(GPU Engine 计数器,两种口径都提供)
                for (code, eng_name, _) in ENGINE_SENSORS {
                    b = b.dynamic(code, eng_name, "%", ValueType::F64);
                }

                let nvml = if vendor_id == 0x10DE { gpu_nvml::try_load() } else { None };
                // Intel 核显且 IGCL 就绪:按能力集声明完整传感器
                let igcl_paired = vendor_id == 0x8086
                    && igcl_caps.is_some()
                    && igcl_dids.contains(&(desc.DeviceId as u16));
                if igcl_paired {
                    let caps = igcl_caps.unwrap();
                    if caps.temperature {
                        b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64);
                    }
                    if caps.energy {
                        b = b.dynamic("power_w", "Power", "W", ValueType::F64);
                    }
                    if caps.voltage {
                        b = b.dynamic("voltage_v", "Voltage", "V", ValueType::F64);
                    }
                    if caps.clock {
                        b = b.dynamic("clock_core_mhz", "Core Clock", "MHz", ValueType::F64);
                    }
                    // util_gpu_pct 已在上面无条件声明;这里补渲染/媒体引擎占用
                    if caps.activity_render {
                        b = b.dynamic("util_render_pct", "Render Usage", "%", ValueType::F64);
                    }
                    if caps.activity_media {
                        b = b.dynamic("util_media_pct", "Media Usage", "%", ValueType::F64);
                    }
                    // IGCL 能耗计数缺失时的功耗回退:RAPL PP1(GT 域)经驱动层差分
                    if !caps.energy && driver::is_available() {
                        b = b.dynamic("power_gt_w", "GPU Power (GT)", "W", ValueType::F64);
                    }
                } else if nvml.is_some() {
                    b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64)
                        .dynamic("fan_pct", "Fan", "%", ValueType::F64)
                        .dynamic("power_w", "Power", "W", ValueType::F64)
                        .dynamic("power_limit_w", "Power Limit", "W", ValueType::F64)
                        .dynamic("clock_core_mhz", "Core Clock", "MHz", ValueType::F64)
                        .dynamic("clock_mem_mhz", "Memory Clock", "MHz", ValueType::F64);
                } else {
                    // 非 NVIDIA:AMD 且 ADLX 可用时声明完整传感器集;其余仅温度占位
                    match adlx_idx.and_then(gpu_adlx::caps) {
                        Some(caps) => {
                            if caps.temperature {
                                b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64);
                            }
                            if caps.hotspot {
                                b = b.dynamic("temperature_hotspot_c", "Hotspot Temperature", "°C", ValueType::F64);
                            }
                            if caps.power {
                                b = b.dynamic("power_w", "Power", "W", ValueType::F64);
                            }
                            if caps.board_power {
                                b = b.dynamic("power_board_w", "Total Board Power", "W", ValueType::F64);
                            }
                            if caps.fan {
                                b = b.dynamic("fan_rpm", "Fan", "RPM", ValueType::F64);
                            }
                            if caps.clock {
                                b = b.dynamic("clock_core_mhz", "Core Clock", "MHz", ValueType::F64);
                            }
                            if caps.vram_clock {
                                b = b.dynamic("clock_mem_mhz", "Memory Clock", "MHz", ValueType::F64);
                            }
                            if caps.voltage {
                                b = b.dynamic("voltage_v", "Voltage", "V", ValueType::F64);
                            }
                        }
                        None => {
                            b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64);
                        }
                    }
                }

                adapters3.push(adapter.cast::<IDXGIAdapter3>().ok());
                devices.push(b.build());
                device_vendor.push(vendor_id);
                adlx_map.push(adlx_idx);
                intel_igcl.push(igcl_paired);
                luids.push((
                    desc.AdapterLuid.HighPart as u32,
                    desc.AdapterLuid.LowPart,
                ));
            }

            // 回退配对:PNP 全部未匹配且 AMD 设备数与 ADLX GPU 数一致时按顺序配对
            if !adlx_gpus.is_empty() && adlx_map.iter().all(|m| m.is_none()) {
                let amd_devs: Vec<usize> = device_vendor
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| **v == 0x1002)
                    .map(|(i, _)| i)
                    .collect();
                if amd_devs.len() == adlx_gpus.len() {
                    for (k, di) in amd_devs.iter().enumerate() {
                        adlx_map[*di] = Some(adlx_gpus[k].2);
                    }
                }
            }

            if devices.is_empty() {
                return Err("未发现硬件 GPU 适配器".into());
            }

            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.adapters = adapters3;
            st.adlx_map = adlx_map;
            st.intel_igcl = intel_igcl;
            st.luids = luids;
            Ok(devices)
        }
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let nvml = gpu_nvml::try_load();
        let (adapters, adlx_map, intel_igcl_devs) = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (st.adapters.clone(), st.adlx_map.clone(), st.intel_igcl.clone())
        };
        // 任务管理器口径:util_gpu_pct 改由 GPU Engine 计数器聚合,
        // 厂商 API 的利用率写入被跳过(其余字段照常)
        let tm = crate::options::usage_mode() == UsageMode::TaskManager;

        for (i, dev) in devices.iter_mut().enumerate() {
            // DXGI 显存(QueryVideoMemoryInfo,node 0 = LOCAL 段,任意厂商)。
            // 注意:部分 AMD 驱动 CurrentUsage 恒为 0 —— 此时标记 NOT_SUPPORTED,
            // 不展示误导性的 0 值。
            if let Some(a3) = adapters.get(i).and_then(|o| o.as_ref()) {
                unsafe {
                    let mut info = Default::default();
                    if a3
                        .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                        .is_ok()
                    {
                        let used_mb = info.CurrentUsage / 1024 / 1024;
                        if used_mb > 0 {
                            dev.set_reading("mem_used_mb", Reading::now(Value::F64(used_mb as f64)));
                            let vram_total = dev
                                .sensors
                                .iter()
                                .find(|s| s.desc.code == "vram_total_mb")
                                .and_then(|s| s.current.value.as_f64())
                                .unwrap_or(0.0);
                            if vram_total > 0.0 {
                                dev.set_reading(
                                    "mem_used_pct",
                                    Reading::now(Value::F64(used_mb as f64 / vram_total * 100.0)),
                                );
                            }
                        } else {
                            dev.set_reading(
                                "mem_used_mb",
                                Reading::failed(Status::NotSupported, ValueType::F64),
                            );
                            dev.set_reading(
                                "mem_used_pct",
                                Reading::failed(Status::NotSupported, ValueType::F64),
                            );
                        }
                    } else {
                        dev.set_reading(
                            "mem_used_mb",
                            Reading::failed(Status::NotSupported, ValueType::F64),
                        );
                        dev.set_reading(
                            "mem_used_pct",
                            Reading::failed(Status::NotSupported, ValueType::F64),
                        );
                    }
                }
            }

            // 利用率与厂商传感器
            let is_nvidia = dev.meta.vendor == "NVIDIA";
            let is_intel_igcl = dev.meta.vendor == "Intel" && intel_igcl_devs.get(i).copied().unwrap_or(false);
            if is_nvidia {
                match nvml {
                    Some(nv) => {
                        // NVML 索引按顺序配对(单 GPU 场景总是正确;多 GPU 记录限制)
                        if let Some(h) = nv.device_handle(i as u32) {
                            if let Some(t) = nv.temperature(h) {
                                dev.set_reading("temperature_c", Reading::now(Value::F64(t)));
                            }
                            if let Some(f) = nv.fan_speed(h) {
                                dev.set_reading("fan_pct", Reading::now(Value::F64(f)));
                            }
                            if let Some(p) = nv.power_usage_w(h) {
                                dev.set_reading(
                                    "power_w",
                                    Reading::now(Value::F64((p * 100.0).round() / 100.0)),
                                );
                            }
                            if let Some(p) = nv.power_limit_w(h) {
                                dev.set_reading("power_limit_w", Reading::now(Value::F64(p)));
                            }
                            if !tm {
                                if let Some((g, _m)) = nv.utilization(h) {
                                    dev.set_reading("util_gpu_pct", Reading::now(Value::F64(g)));
                                }
                            }
                            if let Some((used, total)) = nv.memory(h) {
                                dev.set_reading(
                                    "mem_used_mb",
                                    Reading::now(Value::F64(used as f64)),
                                );
                                if total > 0 {
                                    dev.set_reading(
                                        "mem_used_pct",
                                        Reading::now(Value::F64(used as f64 / total as f64 * 100.0)),
                                    );
                                }
                            }
                            if let Some(c) = nv.clock_mhz(h, gpu_nvml::NVML_CLOCK_GRAPHICS) {
                                dev.set_reading("clock_core_mhz", Reading::now(Value::F64(c as f64)));
                            }
                            if let Some(c) = nv.clock_mhz(h, gpu_nvml::NVML_CLOCK_MEM) {
                                dev.set_reading("clock_mem_mhz", Reading::now(Value::F64(c as f64)));
                            }
                        }
                    }
                    None => {
                        dev.set_reading(
                            "temperature_c",
                            Reading::failed(Status::NoDriver, ValueType::F64),
                        );
                        dev.set_reading(
                            "util_gpu_pct",
                            Reading::failed(Status::NoDriver, ValueType::F64),
                        );
                    }
                }
            } else if is_intel_igcl {
                // Intel 核显:IGCL 遥测(温度/功耗/电压/频率/占用差分)。
                // 读失败(设备瞬入 D3 等)时保留上次值,不覆盖为错误。
                if let Some(t) = super::gpu_igcl::read() {
                    if let Some(v) = t.temperature_c {
                        dev.set_reading("temperature_c", Reading::now(Value::F64(v)));
                    }
                    if let Some(v) = t.power_w {
                        dev.set_reading("power_w", Reading::now(Value::F64(v)));
                    }
                    if let Some(v) = t.voltage_v {
                        dev.set_reading("voltage_v", Reading::now(Value::F64((v * 1000.0).round() / 1000.0)));
                    }
                    if let Some(v) = t.clock_core_mhz {
                        dev.set_reading("clock_core_mhz", Reading::now(Value::F64(v)));
                    }
                    if let Some(v) = if tm { None } else { t.util_global_pct } {
                        dev.set_reading("util_gpu_pct", Reading::now(Value::F64(v.clamp(0.0, 100.0))));
                    }
                    if let Some(v) = t.util_render_pct {
                        dev.set_reading("util_render_pct", Reading::now(Value::F64(v)));
                    }
                    if let Some(v) = t.util_media_pct {
                        dev.set_reading("util_media_pct", Reading::now(Value::F64(v)));
                    }
                }
                // IGCL 能耗缺失时的功耗回退(probe 声明了 power_gt_w 才会写入)
                if driver::is_available() {
                    driver::refresh_gt_power(dev);
                }
            } else {
                // 非 NVIDIA:AMD 走 ADLX(可用时);其余无用户态厂商传感器源
                match adlx_map.get(i).copied().flatten() {
                    Some(ai) => {
                        if let Some(m) = gpu_adlx::read_metrics(ai) {
                            if let Some(t) = m.temperature_c {
                                dev.set_reading(
                                    "temperature_c",
                                    Reading::now(Value::F64((t * 10.0).round() / 10.0)),
                                );
                            }
                            if let Some(h) = m.hotspot_c {
                                dev.set_reading(
                                    "temperature_hotspot_c",
                                    Reading::now(Value::F64((h * 10.0).round() / 10.0)),
                                );
                            }
                            // 利用率:标准口径取 ADLX;任务管理器口径由
                            // GPU Engine 计数器聚合(refresh 末尾统一写入)
                            if !tm {
                                if let Some(u) = m.usage_pct {
                                    dev.set_reading(
                                        "util_gpu_pct",
                                        Reading::now(Value::F64(u.clamp(0.0, 100.0))),
                                    );
                                }
                            }
                            if let Some(p) = m.power_w {
                                dev.set_reading(
                                    "power_w",
                                    Reading::now(Value::F64((p * 100.0).round() / 100.0)),
                                );
                            }
                            if let Some(p) = m.board_power_w {
                                dev.set_reading(
                                    "power_board_w",
                                    Reading::now(Value::F64((p * 100.0).round() / 100.0)),
                                );
                            }
                            if let Some(rpm) = m.fan_rpm {
                                dev.set_reading("fan_rpm", Reading::now(Value::F64(rpm)));
                            }
                            if let Some(c) = m.clock_core_mhz {
                                dev.set_reading("clock_core_mhz", Reading::now(Value::F64(c)));
                            }
                            if let Some(c) = m.clock_mem_mhz {
                                dev.set_reading("clock_mem_mhz", Reading::now(Value::F64(c)));
                            }
                            if let Some(v) = m.voltage_mv {
                                dev.set_reading(
                                    "voltage_v",
                                    Reading::now(Value::F64((v / 1000.0 * 1000.0).round() / 1000.0)),
                                );
                            }
                            if let Some(used) = m.vram_used_mb {
                                let missing = dev
                                    .sensors
                                    .iter()
                                    .find(|s| s.desc.code == "mem_used_mb")
                                    .map(|s| s.current.status != Status::Ok)
                                    .unwrap_or(false);
                                if missing {
                                    dev.set_reading(
                                        "mem_used_mb",
                                        Reading::now(Value::F64(used)),
                                    );
                                    let vram_total = dev
                                        .sensors
                                        .iter()
                                        .find(|s| s.desc.code == "vram_total_mb")
                                        .and_then(|s| s.current.value.as_f64())
                                        .unwrap_or(0.0);
                                    if vram_total > 0.0 {
                                        dev.set_reading(
                                            "mem_used_pct",
                                            Reading::now(Value::F64(used / vram_total * 100.0)),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        dev.set_reading(
                            "util_gpu_pct",
                            Reading::failed(Status::NotSupported, ValueType::F64),
                        );
                        if dev
                            .sensors
                            .iter()
                            .any(|s| s.desc.code == "temperature_c")
                        {
                            dev.set_reading(
                                "temperature_c",
                                Reading::failed(Status::NotSupported, ValueType::F64),
                            );
                        }
                    }
                }
            }
        }

        // 分引擎占用(GPU Engine 计数器):两种口径都写;任务管理器口径
        // 在这里以"最忙引擎类型"覆盖 util_gpu_pct(厂商写入已在上方跳过)
        self.refresh_engines(devices);
    }
}
