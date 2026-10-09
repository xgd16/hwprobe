//! ADLX(AMD Device Library eXtra)运行时接入:AMD GPU 温度/热点/利用率/显存/功耗。
//!
//! 绑定直接 vendor AMD 官方 SDK 中的 Rust crate `amd-adlx`(MIT,来自
//! GPUOpen-LibrariesAndSDKs/ADLX 的 `Samples/rust/amd-adlx`),它从系统
//! `amdadlx64.dll`(随 Adrenalin 驱动安装)运行时加载,不需要链接库。
//!
//! 初始化一次后常驻(与进程同生命周期,不做释放,避免接口释放顺序问题);
//! 所有调用经全局 Mutex 串行化。任何一步失败 → 返回 None,相应传感器保持
//! 无数据源状态(不显示假值)。

#![cfg(target_os = "windows")]

use amd_adlx::*;
use crate::logbuf;
use std::ffi::CStr;
use std::sync::{Mutex, OnceLock};

/// 单个 ADLX GPU 及其指标支持情况(初始化时查询一次)。
struct AdlxGpu {
    name: String,
    /// 规范化后的 PNP 键(用于与 DXGI/WMI 设备配对)。
    pnp_key: String,
    gpu: *mut IADLXGPU,
    support: GpuSupport,
}

#[derive(Default, Clone, Copy)]
struct GpuSupport {
    usage: bool,
    clock: bool,
    vram_clock: bool,
    temperature: bool,
    hotspot: bool,
    power: bool,
    board_power: bool,
    fan: bool,
    vram: bool,
    voltage: bool,
}

pub struct Adlx {
    pm: *mut IADLXPerformanceMonitoringServices,
    gpus: Vec<AdlxGpu>,
    /// 持有 ADLX 运行时句柄;进程内常驻,不触发 Drop。
    _helper: AdlxHelper,
}

// ADLX 接口指针只在本模块的 Mutex 保护下使用。
unsafe impl Send for Adlx {}

static ADLX: OnceLock<Option<Mutex<Adlx>>> = OnceLock::new();

/// 规范化 PNP 设备 ID:大写、去空格,取前两段(PCI\VEN_...&DEV_...&SUBSYS_...&REV_...)。
pub fn pnp_key(pnp: &str) -> String {
    let up = pnp.to_ascii_uppercase().replace(' ', "");
    up.split('\\').take(2).collect::<Vec<_>>().join("\\")
}

/// 惰性初始化(只尝试一次,结果缓存)。返回是否可用。
pub fn ensure_init() -> bool {
    ADLX
        .get_or_init(|| match init_inner() {
            Ok(a) => {
                logbuf::info(format!(
                    "ADLX 就绪({} 个 AMD GPU: {})",
                    a.gpus.len(),
                    a.gpus
                        .iter()
                        .map(|g| g.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                Some(Mutex::new(a))
            }
            Err(e) => {
                logbuf::info(format!("ADLX 不可用,AMD GPU 传感器降级: {e}"));
                None
            }
        })
        .is_some()
}

fn init_inner() -> Result<Adlx, String> {
    let helper = AdlxHelper::new().map_err(|r| format!("ADLXInitialize 失败(0x{r:X})"))?;
    let system = helper.system_services();
    if system.is_null() {
        return Err("ADLX system 接口为空".into());
    }

    let system_vt = unsafe { &*(*system).pVtbl };

    let mut pm: *mut IADLXPerformanceMonitoringServices = std::ptr::null_mut();
    let res = unsafe { (system_vt.GetPerformanceMonitoringServices)(system, &mut pm) };
    if !adlx_succeeded(res) || pm.is_null() {
        return Err(format!("性能监控服务不可用(0x{res:X})"));
    }
    let pm_vt = unsafe { &*(*pm).pVtbl };

    let mut list: *mut IADLXGPUList = std::ptr::null_mut();
    let res = unsafe { (system_vt.GetGPUs)(system, &mut list) };
    if !adlx_succeeded(res) || list.is_null() {
        return Err(format!("GPU 列表不可用(0x{res:X})"));
    }
    let list_vt = unsafe { &*(*list).pVtbl };
    let count = unsafe { (list_vt.Size)(list) };

    let mut gpus = Vec::new();
    for i in 0..count {
        let mut gpu: *mut IADLXGPU = std::ptr::null_mut();
        let res = unsafe { (list_vt.At_GPUList)(list, i, &mut gpu) };
        if !adlx_succeeded(res) || gpu.is_null() {
            continue;
        }
        let gpu_vt = unsafe { &*(*gpu).pVtbl };

        let name = read_cstr(|out| unsafe { (gpu_vt.Name)(gpu, out) }).unwrap_or_default();
        let pnp = read_cstr(|out| unsafe { (gpu_vt.PNPString)(gpu, out) }).unwrap_or_default();

        // 指标支持情况
        let mut support = GpuSupport::default();
        let mut sup: *mut IADLXGPUMetricsSupport = std::ptr::null_mut();
        let res = unsafe { (pm_vt.GetSupportedGPUMetrics)(pm, gpu, &mut sup) };
        if adlx_succeeded(res) && !sup.is_null() {
            let sv = unsafe { &*(*sup).pVtbl };
            let q = |f: unsafe extern "system" fn(
                *mut IADLXGPUMetricsSupport,
                *mut adlx_bool,
            ) -> ADLX_RESULT|
             -> bool {
                let mut v: adlx_bool = 0;
                let r = unsafe { f(sup, &mut v) };
                adlx_succeeded(r) && v != 0
            };
            support.usage = q(sv.IsSupportedGPUUsage);
            support.clock = q(sv.IsSupportedGPUClockSpeed);
            support.vram_clock = q(sv.IsSupportedGPUVRAMClockSpeed);
            support.temperature = q(sv.IsSupportedGPUTemperature);
            support.hotspot = q(sv.IsSupportedGPUHotspotTemperature);
            support.power = q(sv.IsSupportedGPUPower);
            support.board_power = q(sv.IsSupportedGPUTotalBoardPower);
            support.fan = q(sv.IsSupportedGPUFanSpeed);
            support.vram = q(sv.IsSupportedGPUVRAM);
            support.voltage = q(sv.IsSupportedGPUVoltage);
            // 释放 support 引用
            unsafe {
                (sv.Release)(sup);
            }
        }

        gpus.push(AdlxGpu {
            name,
            pnp_key: pnp_key(&pnp),
            gpu,
            support,
        });
    }

    // 列表引用用完即释放;各 IADLXGPU 是独立引用计数,按模块设计常驻保留
    unsafe {
        (list_vt.Release)(list);
    }

    Ok(Adlx {
        pm,
        gpus,
        _helper: helper,
    })
}

fn read_cstr(
    f: impl FnOnce(*mut *const std::os::raw::c_char) -> ADLX_RESULT,
) -> Option<String> {
    let mut p: *const std::os::raw::c_char = std::ptr::null();
    let res = f(&mut p);
    if !adlx_succeeded(res) || p.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// 单个 GPU 的一次指标采样。
#[derive(Default, Clone, Copy)]
pub struct Metrics {
    pub temperature_c: Option<f64>,
    pub hotspot_c: Option<f64>,
    pub usage_pct: Option<f64>,
    pub power_w: Option<f64>,
    pub board_power_w: Option<f64>,
    pub fan_rpm: Option<f64>,
    pub vram_used_mb: Option<f64>,
    pub clock_core_mhz: Option<f64>,
    pub clock_mem_mhz: Option<f64>,
    pub voltage_mv: Option<f64>,
}

/// ADLX GPU 配对信息:名称、PNP 键与序号。
pub fn gpu_list() -> Vec<(String, String, usize)> {
    match ADLX.get().and_then(|o| o.as_ref()) {
        Some(a) => {
            let a = a.lock().unwrap_or_else(|e| e.into_inner());
            a.gpus
                .iter()
                .enumerate()
                .map(|(i, g)| (g.name.clone(), g.pnp_key.clone(), i))
                .collect()
        }
        None => Vec::new(),
    }
}

/// 该 ADLX GPU 支持的指标集(probe 阶段决定声明哪些传感器)。
#[derive(Default, Clone, Copy)]
pub struct GpuCaps {
    pub temperature: bool,
    pub hotspot: bool,
    pub power: bool,
    pub board_power: bool,
    pub fan: bool,
    pub clock: bool,
    pub vram_clock: bool,
    pub voltage: bool,
}

/// 查询指定 ADLX GPU 的支持能力。
pub fn caps(idx: usize) -> Option<GpuCaps> {
    let m = ADLX.get().and_then(|o| o.as_ref())?;
    let a = m.lock().unwrap_or_else(|e| e.into_inner());
    a.gpus.get(idx).map(|g| GpuCaps {
        temperature: g.support.temperature,
        hotspot: g.support.hotspot,
        power: g.support.power,
        board_power: g.support.board_power,
        fan: g.support.fan,
        clock: g.support.clock,
        vram_clock: g.support.vram_clock,
        voltage: g.support.voltage,
    })
}

/// 采集指定 ADLX GPU 的当前指标。ADLX 不可用或该 GPU 不存在时返回 None。
pub fn read_metrics(idx: usize) -> Option<Metrics> {
    let m = ADLX.get().and_then(|o| o.as_ref())?;
    let a = m.lock().unwrap_or_else(|e| e.into_inner());
    let gpu = a.gpus.get(idx)?;
    let pm_vt = unsafe { &*(*a.pm).pVtbl };

    let mut metrics: *mut IADLXGPUMetrics = std::ptr::null_mut();
    let res = unsafe { (pm_vt.GetCurrentGPUMetrics)(a.pm, gpu.gpu, &mut metrics) };
    if !adlx_succeeded(res) || metrics.is_null() {
        return None;
    }
    let mv = unsafe { &*(*metrics).pVtbl };
    let s = &gpu.support;

    let mut out = Metrics::default();
    unsafe {
        if s.temperature {
            let mut v: adlx_double = 0.0;
            if adlx_succeeded((mv.GPUTemperature)(metrics, &mut v)) {
                out.temperature_c = Some(v);
            }
        }
        if s.hotspot {
            let mut v: adlx_double = 0.0;
            if adlx_succeeded((mv.GPUHotspotTemperature)(metrics, &mut v)) {
                out.hotspot_c = Some(v);
            }
        }
        if s.usage {
            let mut v: adlx_double = 0.0;
            if adlx_succeeded((mv.GPUUsage)(metrics, &mut v)) {
                out.usage_pct = Some(v);
            }
        }
        if s.power {
            let mut v: adlx_double = 0.0;
            if adlx_succeeded((mv.GPUPower)(metrics, &mut v)) {
                out.power_w = Some(v);
            }
        }
        if s.board_power {
            let mut v: adlx_double = 0.0;
            if adlx_succeeded((mv.GPUTotalBoardPower)(metrics, &mut v)) {
                out.board_power_w = Some(v);
            }
        }
        if s.fan {
            let mut v: adlx_int = 0;
            if adlx_succeeded((mv.GPUFanSpeed)(metrics, &mut v)) {
                out.fan_rpm = Some(v as f64);
            }
        }
        if s.vram {
            let mut v: adlx_int = 0;
            if adlx_succeeded((mv.GPUVRAM)(metrics, &mut v)) {
                out.vram_used_mb = Some(v as f64);
            }
        }
        if s.clock {
            let mut v: adlx_int = 0;
            if adlx_succeeded((mv.GPUClockSpeed)(metrics, &mut v)) {
                out.clock_core_mhz = Some(v as f64);
            }
        }
        if s.vram_clock {
            let mut v: adlx_int = 0;
            if adlx_succeeded((mv.GPUVRAMClockSpeed)(metrics, &mut v)) {
                out.clock_mem_mhz = Some(v as f64);
            }
        }
        if s.voltage {
            let mut v: adlx_int = 0;
            if adlx_succeeded((mv.GPUVoltage)(metrics, &mut v)) {
                out.voltage_mv = Some(v as f64);
            }
        }
        // 释放指标对象
        (mv.Release)(metrics);
    }
    Some(out)
}
