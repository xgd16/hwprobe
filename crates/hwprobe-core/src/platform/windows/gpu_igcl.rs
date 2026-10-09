//! Intel IGCL(Intel Graphics Control Library,`ControlLib.dll`)遥测:
//! Intel 核显/独显的温度、电压、频率、能耗与活跃度计数。
//!
//! MSI Claw A1M 等 Intel 平台掌机的核显(Arc Graphics / Xe-LPG)没有
//! NVML/ADLX 对应物,温度/功耗只能经 IGCL 取得;驱动包随附的 ControlLib.dll
//! 位于 System32,运行时加载,缺失自动降级。
//!
//! ABI 按 IGCL 官方头 `igcl_api.h`(intel/drivers.gpu.control-library,C++ 编译,
//! `bool` 为 1 字节)。结构体布局用 const 断言固定;官方头文件里
//! `ctl_oc_telemetry_item_t` 的 bSupported(units/type/value 之前)无论按
//! 1 字节 C++ bool 还是 4 字节 BOOL 封送,字段偏移一致(union 8 字节对齐吸收),
//! 而本实现按真实 ABI(1 字节)定义,使 `vram*`/`fanSpeed`/`psu` 段同样正确。
//!
//! 数据语义(igcl_api.h):
//! - `gpuEnergyCounter` 累计焦耳 → 差分得功率;
//! - `globalActivityCounter` 等为"繁忙秒数"累计 → 差分得利用率百分比。

#![cfg(target_os = "windows")]

use crate::logbuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// ---------------- 常量(igcl_api.h) ----------------

/// (主版本 << 16) | 次版本。驱动校验 AppVersion 不高于自身实现版本。
const CTL_IMPL_VERSION: u32 = (1 << 16) | 1;
/// 遥测必需(LibreHardwareMonitor 同款注释:Required for telemetry)。
const CTL_INIT_FLAG_USE_LEVEL_ZERO: u32 = 1 << 0;
const CTL_DEVICE_TYPE_GRAPHICS: u32 = 1;
/// graphics_adapter_properties 位:核显。
const CTL_ADAPTER_PROPERTIES_FLAG_INTEGRATED: u32 = 1 << 0;
const CTL_RESULT_SUCCESS: i32 = 0;
/// 已被其他进程初始化(仍算成功,句柄可用)。
const CTL_RESULT_SUCCESS_STILL_OPEN: i32 = 1;

// ctl_data_type_t
const DT_INT8: u32 = 0;
const DT_UINT8: u32 = 1;
const DT_INT16: u32 = 2;
const DT_UINT16: u32 = 3;
const DT_INT32: u32 = 4;
const DT_UINT32: u32 = 5;
const DT_INT64: u32 = 6;
const DT_UINT64: u32 = 7;
const DT_FLOAT: u32 = 8;
const DT_DOUBLE: u32 = 9;

const CTL_MAX_DEVICE_NAME_LEN: usize = 100;
const CTL_MAX_RESERVED_SIZE: usize = 112;
const CTL_PSU_COUNT: usize = 5;
const CTL_FAN_COUNT: usize = 5;
const MAX_DEVICES: usize = 64;

// ---------------- 结构体(严格按 C++ ABI 布局) ----------------

#[repr(C)]
#[derive(Clone, Copy)]
union CtlDataValue {
    data8: i8,
    datau8: u8,
    data16: i16,
    datau16: u16,
    data32: i32,
    datau32: u32,
    data64: i64,
    datau64: u64,
    datafloat: f32,
    datadouble: f64,
}

const _: () = assert!(std::mem::size_of::<CtlDataValue>() == 8);
const _: () = assert!(std::mem::align_of::<CtlDataValue>() == 8);

/// ctl_oc_telemetry_item_t。
#[repr(C)]
#[derive(Clone, Copy)]
struct CtlTelemetryItem {
    supported: bool,
    units: u32,
    dtype: u32,
    value: CtlDataValue,
}

const _: () = assert!(std::mem::size_of::<CtlTelemetryItem>() == 24);
const _: () = assert!(std::mem::offset_of!(CtlTelemetryItem, units) == 4);
const _: () = assert!(std::mem::offset_of!(CtlTelemetryItem, value) == 16);

#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPsuInfo {
    supported: bool,
    psu_type: u32,
    energy_counter: CtlTelemetryItem,
    voltage: CtlTelemetryItem,
}

const _: () = assert!(std::mem::size_of::<CtlPsuInfo>() == 56);

/// ctl_power_telemetry_t(C++ ABI:bool 1 字节,总大小 1024)。
#[repr(C)]
struct CtlPowerTelemetry {
    size: u32,
    version: u8,
    time_stamp: CtlTelemetryItem,             // 8
    gpu_energy_counter: CtlTelemetryItem,     // 32
    gpu_voltage: CtlTelemetryItem,            // 56
    gpu_current_clock_frequency: CtlTelemetryItem, // 80
    gpu_current_temperature: CtlTelemetryItem,     // 104
    global_activity_counter: CtlTelemetryItem,     // 128
    render_compute_activity_counter: CtlTelemetryItem, // 152
    media_activity_counter: CtlTelemetryItem,      // 176
    gpu_power_limited: bool,                       // 200
    gpu_temperature_limited: bool,
    gpu_current_limited: bool,
    gpu_voltage_limited: bool,
    gpu_utilization_limited: bool,                 // 204
    vram_energy_counter: CtlTelemetryItem,         // 208
    vram_voltage: CtlTelemetryItem,
    vram_current_clock_frequency: CtlTelemetryItem,
    vram_current_effective_frequency: CtlTelemetryItem,
    vram_read_bandwidth_counter: CtlTelemetryItem,
    vram_write_bandwidth_counter: CtlTelemetryItem,
    vram_current_temperature: CtlTelemetryItem,    // 352
    vram_power_limited: bool,
    vram_temperature_limited: bool,
    vram_current_limited: bool,
    vram_voltage_limited: bool,
    vram_utilization_limited: bool,                // 380
    total_card_energy_counter: CtlTelemetryItem,   // 384
    psu: [CtlPsuInfo; CTL_PSU_COUNT],              // 408
    fan_speed: [CtlTelemetryItem; CTL_FAN_COUNT],  // 688
    gpu_vr_temp: CtlTelemetryItem,                 // 808
    vram_vr_temp: CtlTelemetryItem,
    sa_vr_temp: CtlTelemetryItem,
    gpu_effective_clock: CtlTelemetryItem,
    gpu_over_voltage_percent: CtlTelemetryItem,
    gpu_power_percent: CtlTelemetryItem,
    gpu_temperature_percent: CtlTelemetryItem,
    vram_read_bandwidth: CtlTelemetryItem,
    vram_write_bandwidth: CtlTelemetryItem,
}

const _: () = assert!(std::mem::size_of::<CtlPowerTelemetry>() == 1024);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, gpu_energy_counter) == 32);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, gpu_current_temperature) == 104);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, global_activity_counter) == 128);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, vram_energy_counter) == 208);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, total_card_energy_counter) == 384);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, psu) == 408);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, fan_speed) == 688);
const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetry, gpu_effective_clock) == 880);

#[repr(C)]
struct CtlFirmwareVersion {
    major: u64,
    minor: u64,
    build: u64,
}

#[repr(C)]
struct CtlAdapterBdf {
    bus: u8,
    device: u8,
    function: u8,
}

/// ctl_device_adapter_properties_t(Version 2)。
#[repr(C)]
struct CtlDeviceAdapterProperties {
    size: u32,
    version: u8,
    p_device_id: *mut std::ffi::c_void,
    device_id_size: u32,
    device_type: u32,
    supported_subfunction_flags: u32,
    driver_version: u64,
    firmware_version: CtlFirmwareVersion,
    pci_vendor_id: u32,
    pci_device_id: u32,
    rev_id: u32,
    num_eus_per_sub_slice: u32,
    num_sub_slices_per_slice: u32,
    num_slices: u32,
    name: [u8; CTL_MAX_DEVICE_NAME_LEN],
    graphics_adapter_properties: u32,
    frequency: u32,
    pci_subsys_id: u16,
    pci_subsys_vendor_id: u16,
    adapter_bdf: CtlAdapterBdf,
    num_xe_cores: u32,
    reserved: [u8; CTL_MAX_RESERVED_SIZE],
}

const _: () = assert!(std::mem::size_of::<CtlDeviceAdapterProperties>() == 320);
const _: () = assert!(std::mem::offset_of!(CtlDeviceAdapterProperties, pci_vendor_id) == 64);
const _: () = assert!(std::mem::offset_of!(CtlDeviceAdapterProperties, name) == 88);
const _: () = assert!(std::mem::offset_of!(CtlDeviceAdapterProperties, graphics_adapter_properties) == 188);
const _: () = assert!(std::mem::offset_of!(CtlDeviceAdapterProperties, num_xe_cores) == 204);

/// ctl_init_args_t。
#[repr(C)]
struct CtlInitArgs {
    size: u32,
    version: u8,
    app_version: u32,
    flags: u32,
    supported_version: u32,
    application_uid: [u8; 16],
}

const _: () = assert!(std::mem::size_of::<CtlInitArgs>() == 36);

// ---------------- 函数指针(libloading 运行时加载) ----------------

type FnCtlInit = unsafe extern "system" fn(*mut CtlInitArgs, *mut *mut std::ffi::c_void) -> i32;
type FnCtlEnumerateDevices = unsafe extern "system" fn(
    *mut std::ffi::c_void,
    *mut u32,
    *mut *mut std::ffi::c_void,
) -> i32;
type FnCtlGetDeviceProperties = unsafe extern "system" fn(
    *mut std::ffi::c_void,
    *mut CtlDeviceAdapterProperties,
) -> i32;
type FnCtlPowerTelemetryGet =
    unsafe extern "system" fn(*mut std::ffi::c_void, *mut CtlPowerTelemetry) -> i32;
type FnCtlClose = unsafe extern "system" fn(*mut std::ffi::c_void) -> i32;

struct IgclApi {
    init: FnCtlInit,
    enumerate_devices: FnCtlEnumerateDevices,
    get_device_properties: FnCtlGetDeviceProperties,
    power_telemetry_get: FnCtlPowerTelemetryGet,
    /// 进程生命周期内不关 API(Provider 常驻),保留句柄供未来显式关闭。
    #[allow(dead_code)]
    close: FnCtlClose,
}

/// ControlLib.dll 的句柄随进程保留(库生命周期 = 进程生命周期)。
static API: OnceLock<Option<IgclApi>> = OnceLock::new();

fn load_api() -> Option<IgclApi> {
    // Intel 驱动把 ControlLib.dll 安装到 System32;libloading 依赖默认 DLL 搜索。
    unsafe {
        let lib = libloading::Library::new("ControlLib.dll").ok()?;
        let init: FnCtlInit = *lib.get(b"ctlInit\0").ok()?;
        let enumerate_devices: FnCtlEnumerateDevices = *lib.get(b"ctlEnumerateDevices\0").ok()?;
        let get_device_properties: FnCtlGetDeviceProperties =
            *lib.get(b"ctlGetDeviceProperties\0").ok()?;
        let power_telemetry_get: FnCtlPowerTelemetryGet =
            *lib.get(b"ctlPowerTelemetryGet\0").ok()?;
        let close: FnCtlClose = *lib.get(b"ctlClose\0").ok()?;
        // 全部函数指针已取出(语句结束即释放借用),泄漏句柄与进程同生命周期,
        // 避免卸载后悬挂。
        std::mem::forget(lib);
        Some(IgclApi {
            init,
            enumerate_devices,
            get_device_properties,
            power_telemetry_get,
            close,
        })
    }
}

fn api() -> Option<&'static IgclApi> {
    API.get_or_init(load_api).as_ref()
}

// ---------------- 设备枚举与遥测 ----------------

struct IgclDevice {
    /// ctl_device_adapter_handle_t(内部指针,跨线程可用)。
    handle: *mut std::ffi::c_void,
    /// PCI 设备 ID(与 DXGI 适配器配对用)。
    pci_device_id: u16,
    name: String,
    #[allow(dead_code)]
    num_xe_cores: u32,
}

unsafe impl Send for IgclDevice {}
unsafe impl Sync for IgclDevice {}

struct IgclState {
    api: &'static IgclApi,
    #[allow(dead_code)]
    api_handle: *mut std::ffi::c_void,
    /// 核显(CTL_ADAPTER_PROPERTIES_FLAG_INTEGRATED)设备列表,通常 1 个。
    integrated: Vec<IgclDevice>,
}

unsafe impl Send for IgclState {}
unsafe impl Sync for IgclState {}

static STATE: OnceLock<Option<IgclState>> = OnceLock::new();
/// 首次遥测采样得到的能力集(哪些字段 bSupported)。
static CAPS: OnceLock<IgclCaps> = OnceLock::new();
/// 能量/活跃度差分状态。
static DELTA: Mutex<DeltaState> = Mutex::new(DeltaState::new());

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IgclCaps {
    pub temperature: bool,
    pub clock: bool,
    pub voltage: bool,
    pub energy: bool,
    pub activity_global: bool,
    pub activity_render: bool,
    pub activity_media: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IgclTelemetry {
    pub temperature_c: Option<f64>,
    pub clock_core_mhz: Option<f64>,
    pub voltage_v: Option<f64>,
    /// GPU 瞬时功率 W(能量差分)。
    pub power_w: Option<f64>,
    /// GPU 总利用率 %(活跃度差分)。
    pub util_global_pct: Option<f64>,
    /// 3D/Compute 利用率 %。
    pub util_render_pct: Option<f64>,
    /// 媒体引擎利用率 %。
    pub util_media_pct: Option<f64>,
}

#[derive(Debug, Default)]
struct DeltaState {
    last_energy_j: Option<f64>,
    last_busy_s: [Option<f64>; 3],
    last_at: Option<Instant>,
}

impl DeltaState {
    const fn new() -> Self {
        DeltaState {
            last_energy_j: None,
            last_busy_s: [None, None, None],
            last_at: None,
        }
    }
}

/// 加载 IGCL 并枚举核显设备。失败静默(None),调用方按无 IGCL 处理。
fn ensure_state() -> Option<&'static IgclState> {
    STATE.get_or_init(|| {
        let api = api()?;
        unsafe {
            let mut init_args = CtlInitArgs {
                size: std::mem::size_of::<CtlInitArgs>() as u32,
                version: 0,
                app_version: CTL_IMPL_VERSION,
                flags: CTL_INIT_FLAG_USE_LEVEL_ZERO,
                supported_version: 0,
                application_uid: [0; 16],
            };
            let mut api_handle: *mut std::ffi::c_void = std::ptr::null_mut();
            let r = (api.init)(&mut init_args, &mut api_handle);
            if (r != CTL_RESULT_SUCCESS && r != CTL_RESULT_SUCCESS_STILL_OPEN)
                || api_handle.is_null()
            {
                logbuf::info(format!("IGCL ctlInit 失败: 0x{r:08X}"));
                return None;
            }

            // 两次调用:先取数量,再取句柄
            let mut count: u32 = 0;
            let r = (api.enumerate_devices)(api_handle, &mut count, std::ptr::null_mut());
            if r != CTL_RESULT_SUCCESS || count == 0 {
                logbuf::info(format!("IGCL 无设备(0x{r:08X})"));
                return None;
            }
            let count = (count as usize).min(MAX_DEVICES);
            let mut handles = vec![std::ptr::null_mut::<std::ffi::c_void>(); count];
            let mut count2 = count as u32;
            let r = (api.enumerate_devices)(api_handle, &mut count2, handles.as_mut_ptr());
            if r != CTL_RESULT_SUCCESS {
                logbuf::info(format!("IGCL 设备枚举失败: 0x{r:08X}"));
                return None;
            }

            let mut integrated = Vec::new();
            for &h in &handles {
                if h.is_null() {
                    continue;
                }
                let mut props = new_properties();
                let r = (api.get_device_properties)(h, &mut props);
                if r != CTL_RESULT_SUCCESS {
                    continue;
                }
                if props.device_type != CTL_DEVICE_TYPE_GRAPHICS {
                    continue;
                }
                if props.graphics_adapter_properties & CTL_ADAPTER_PROPERTIES_FLAG_INTEGRATED == 0 {
                    continue;
                }
                let name = props
                    .name
                    .iter()
                    .position(|&c| c == 0)
                    .map(|i| String::from_utf8_lossy(&props.name[..i]).into_owned())
                    .unwrap_or_default();
                integrated.push(IgclDevice {
                    handle: h,
                    pci_device_id: props.pci_device_id as u16,
                    name,
                    num_xe_cores: props.num_xe_cores,
                });
            }

            if integrated.is_empty() {
                return None;
            }
            let ids: Vec<String> = integrated
                .iter()
                .map(|d| format!("{} (0x{:04X})", d.name, d.pci_device_id))
                .collect();
            logbuf::info(format!("IGCL 核显就绪: {}", ids.join(", ")));
            Some(IgclState {
                api,
                api_handle,
                integrated,
            })
        }
    })
    .as_ref()
}

fn new_properties() -> CtlDeviceAdapterProperties {
    // name/reserved 必须清零(驱动按 Version 检查 Size)。
    let mut props: CtlDeviceAdapterProperties = unsafe { std::mem::zeroed() };
    props.size = std::mem::size_of::<CtlDeviceAdapterProperties>() as u32;
    props.version = 2;
    props
}

/// 声明传感器前取能力集(首次遥测采样)。GPU 瞬入 D3(如息屏)时首次
/// 采样会整体失败 —— 最多重试 3 次(间隔 150ms);仍失败则按空能力集
/// 处理(不声明传感器,宁可少报不误报)。
pub fn caps() -> IgclCaps {
    *CAPS.get_or_init(|| {
        let mut caps = IgclCaps::default();
        for attempt in 0..3 {
            match sample_once() {
                Some((_, c)) => {
                    caps = c;
                    break;
                }
                None => {
                    if attempt + 1 < 3 {
                        std::thread::sleep(std::time::Duration::from_millis(150));
                    }
                }
            }
        }
        if caps == IgclCaps::default() {
            logbuf::warn("IGCL 遥测采样三次均失败(设备可能处于低功耗态),不声明核显传感器");
        } else {
            logbuf::info(format!(
                "IGCL 能力: 温度={} 频率={} 电压={} 能耗={} 活跃度={}",
                caps.temperature, caps.clock, caps.voltage, caps.energy, caps.activity_global
            ));
        }
        caps
    })
}

/// IGCL 是否就绪(核显设备存在)。
pub fn is_integrated_available() -> bool {
    ensure_state().is_some()
}

/// 本平台核显的 PCI Device ID(与 DXGI 适配器配对;多核显返回 None 由调用方按序配对)。
pub fn integrated_pci_device_ids() -> Vec<u16> {
    match ensure_state() {
        Some(st) => st.integrated.iter().map(|d| d.pci_device_id).collect(),
        None => Vec::new(),
    }
}

fn sample_once() -> Option<(IgclTelemetry, IgclCaps)> {
    let st = ensure_state()?;
    let dev = st.integrated.first()?;
    let t = read_telemetry_raw(st.api, dev.handle)?;
    let caps = IgclCaps {
        temperature: t.gpu_current_temperature.supported,
        clock: t.gpu_current_clock_frequency.supported,
        voltage: t.gpu_voltage.supported,
        energy: t.gpu_energy_counter.supported,
        activity_global: t.global_activity_counter.supported,
        activity_render: t.render_compute_activity_counter.supported,
        activity_media: t.media_activity_counter.supported,
    };
    let tel = interpret(&t);
    Some((tel, caps))
}

/// 读取一次遥测并差分出功率/利用率。
pub fn read() -> Option<IgclTelemetry> {
    let st = ensure_state()?;
    let dev = st.integrated.first()?;
    let t = read_telemetry_raw(st.api, dev.handle)?;
    Some(interpret(&t))
}

fn read_telemetry_raw(api: &IgclApi, handle: *mut std::ffi::c_void) -> Option<CtlPowerTelemetry> {
    let mut tel: CtlPowerTelemetry = unsafe { std::mem::zeroed() };
    tel.size = std::mem::size_of::<CtlPowerTelemetry>() as u32;
    tel.version = 1;
    let r = unsafe { (api.power_telemetry_get)(handle, &mut tel) };
    if r != CTL_RESULT_SUCCESS {
        return None;
    }
    Some(tel)
}

/// 从单次采样提取数值(不含差分);无效范围视为不支持,绝不输出假数据。
fn interpret(t: &CtlPowerTelemetry) -> IgclTelemetry {
    let temperature_c = item_f64(&t.gpu_current_temperature).filter(|v| (-20.0..150.0).contains(v));
    let clock_core_mhz =
        item_f64(&t.gpu_current_clock_frequency).filter(|v| (0.0..5000.0).contains(v));
    let voltage_v = item_f64(&t.gpu_voltage).filter(|v| (0.0..2.5).contains(v));
    let energy_j = item_f64(&t.gpu_energy_counter);
    let busy = [
        item_f64(&t.global_activity_counter),
        item_f64(&t.render_compute_activity_counter),
        item_f64(&t.media_activity_counter),
    ];

    let now = Instant::now();
    let mut tel = IgclTelemetry {
        temperature_c,
        clock_core_mhz,
        voltage_v,
        power_w: None,
        util_global_pct: None,
        util_render_pct: None,
        util_media_pct: None,
    };

    let mut delta = DELTA.lock().unwrap_or_else(|e| e.into_inner());
    let dt = delta
        .last_at
        .replace(now)
        .map(|at| now.duration_since(at).as_secs_f64());
    // 间隔过大(probe→首轮刷新、系统休眠恢复等)时能量/活跃度差分不再是
    // 瞬时值,放弃本轮读值并重建基线
    if let Some(dt) = dt.filter(|d| *d <= 10.0) {
        if dt >= 0.05 {
            if let (Some(e0), Some(e1)) = (delta.last_energy_j, energy_j) {
                // 计数器可能回绕或重置:只有正差分才有效
                let de = e1 - e0;
                if de >= 0.0 && dt > 0.0 {
                    let w = de / dt;
                    if (0.0..500.0).contains(&w) {
                        tel.power_w = Some((w * 100.0).round() / 100.0);
                    }
                }
            }
            let names = [
                &mut tel.util_global_pct,
                &mut tel.util_render_pct,
                &mut tel.util_media_pct,
            ];
            for i in 0..3 {
                if let (Some(b0), Some(b1)) = (delta.last_busy_s[i], busy[i]) {
                    let db = b1 - b0;
                    if db >= 0.0 && b1 >= b0 {
                        let pct = db / dt * 100.0;
                        if (0.0..=100.0).contains(&pct) {
                            *names[i] = Some((pct * 10.0).round() / 10.0);
                        }
                    }
                }
            }
        }
    }
    delta.last_energy_j = energy_j;
    delta.last_busy_s = busy;
    tel
}

fn item_f64(item: &CtlTelemetryItem) -> Option<f64> {
    if !item.supported {
        return None;
    }
    let f = unsafe {
        let v = item.value;
        match item.dtype {
            DT_FLOAT => v.datafloat as f64,
            DT_DOUBLE => v.datadouble,
            DT_UINT64 => v.datau64 as f64,
            DT_INT64 => v.data64 as f64,
            DT_UINT32 => v.datau32 as f64,
            DT_INT32 => v.data32 as f64,
            DT_UINT16 => v.datau16 as f64,
            DT_INT16 => v.data16 as f64,
            DT_UINT8 => v.datau8 as f64,
            DT_INT8 => v.data8 as f64,
            _ => return None,
        }
    };
    f.is_finite().then_some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结构体布局必须与官方 C++ ABI 一致(igcl_api.h)。
    #[test]
    fn igcl_layouts() {
        assert_eq!(std::mem::size_of::<CtlDataValue>(), 8);
        assert_eq!(std::mem::size_of::<CtlTelemetryItem>(), 24);
        assert_eq!(std::mem::size_of::<CtlPsuInfo>(), 56);
        assert_eq!(std::mem::size_of::<CtlPowerTelemetry>(), 1024);
        assert_eq!(std::mem::size_of::<CtlDeviceAdapterProperties>(), 320);
        assert_eq!(std::mem::size_of::<CtlInitArgs>(), 36);
        assert_eq!(std::mem::offset_of!(CtlPowerTelemetry, gpu_current_temperature), 104);
        assert_eq!(std::mem::offset_of!(CtlPowerTelemetry, fan_speed), 688);
    }

    /// item 解码:按 dtype 与 supported 取值。
    #[test]
    fn item_decode() {
        let mut item: CtlTelemetryItem = unsafe { std::mem::zeroed() };
        assert_eq!(item_f64(&item), None); // bSupported=false
        item.supported = true;
        item.dtype = DT_FLOAT;
        unsafe { item.value.datafloat = 55.5 };
        assert_eq!(item_f64(&item), Some(55.5));
        item.dtype = DT_UINT64;
        unsafe { item.value.datau64 = 123 };
        assert_eq!(item_f64(&item), Some(123.0));
        item.dtype = 99;
        assert_eq!(item_f64(&item), None);
    }
}
