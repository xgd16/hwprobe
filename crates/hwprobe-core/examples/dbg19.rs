#![cfg(target_os = "windows")]
//! IGCL 温度传感器入口实测(掌机 MSI Claw A1M / Meteor Lake 核显)。
//!
//! 背景:`ctlPowerTelemetryGet` 的 `gpuCurrentTemperature.bSupported = false`,
//! provider 据此不声明 GPU 温度传感器。但 ControlLib.dll 还导出
//! `ctlEnumTemperatureSensors` / `ctlTemperatureGetProperties` /
//! `ctlTemperatureGetState`(igcl_api.h 的 temperature 段),是一条独立通路。
//! 本程序实测该通路在核显上能否真的取到温度,并顺带记录
//! 风扇枚举(ctlEnumFans)、V1/V2 遥测的差异,作为"还有没有别的路"的证据。
//!
//! 用法: dbg19

use libloading::Library;
use std::ffi::c_void;

const CTL_IMPL_VERSION: u32 = (1 << 16) | 1;
const CTL_INIT_FLAG_USE_LEVEL_ZERO: u32 = 1 << 0;
const CTL_DEVICE_TYPE_GRAPHICS: u32 = 1;
const CTL_ADAPTER_PROPERTIES_FLAG_INTEGRATED: u32 = 1 << 0;
const CTL_RESULT_SUCCESS: i32 = 0;
const CTL_RESULT_SUCCESS_STILL_OPEN: i32 = 1;

const CTL_MAX_DEVICE_NAME_LEN: usize = 100;
const CTL_MAX_RESERVED_SIZE: usize = 112;
const MAX_DEVICES: usize = 64;

// ---------------- 结构体(严格按 igcl_api.h 的 C++ ABI) ----------------

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
const _: () = assert!(std::mem::offset_of!(CtlTelemetryItem, value) == 16);

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
    p_device_id: *mut c_void,
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

/// ctl_temp_properties_t:{Size, Version, type, maxTemperature}。
#[repr(C)]
#[derive(Clone, Copy)]
struct CtlTempProperties {
    size: u32,
    version: u8,
    sensor_type: i32,
    max_temperature: f64,
}

const _: () = assert!(std::mem::size_of::<CtlTempProperties>() == 24);
const _: () = assert!(std::mem::offset_of!(CtlTempProperties, max_temperature) == 16);

/// ctl_fan_properties_t。
#[repr(C)]
#[derive(Clone, Copy)]
struct CtlFanProperties {
    size: u32,
    version: u8,
    can_control: bool,
    supported_modes: u32,
    supported_units: u32,
    max_rpm: i32,
    max_points: i32,
}

const _: () = assert!(std::mem::size_of::<CtlFanProperties>() == 24);

/// ctl_power_properties_t。
#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPowerProperties {
    size: u32,
    version: u8,
    can_control: bool,
    default_limit: i32,
    min_limit: i32,
    max_limit: i32,
}

// {u32, u8, bool, i32, i32, i32}:bool 后只剩 2 字节填充,对齐到 4 → 20 字节。
const _: () = assert!(std::mem::size_of::<CtlPowerProperties>() == 20);

/// ctl_power_energy_counter_t:{Size, Version, energy(uJ), timestamp(us)}。
#[repr(C)]
#[derive(Clone, Copy)]
struct CtlPowerEnergyCounter {
    size: u32,
    version: u8,
    energy_uj: u64,
    timestamp_us: u64,
}

const _: () = assert!(std::mem::size_of::<CtlPowerEnergyCounter>() == 24);

/// ctl_power_telemetry_v2_t 的前缀 —— 与 V1 逐字段同布局,到
/// gpuCurrentTemperature 为止。尾部补齐到 V1 的 1024 字节作为越界保护:
/// 调用时 Size 仍填真实的 736,若驱动不认,多写的字节也落在本缓冲内。
#[repr(C)]
struct CtlPowerTelemetryV2 {
    size: u32,
    version: u8,
    time_stamp: CtlTelemetryItem,
    gpu_energy_counter: CtlTelemetryItem,
    gpu_voltage: CtlTelemetryItem,
    gpu_current_clock_frequency: CtlTelemetryItem,
    gpu_current_temperature: CtlTelemetryItem,
    tail: [u8; 1024 - 128],
}

const _: () = assert!(std::mem::offset_of!(CtlPowerTelemetryV2, gpu_current_temperature) == 104);
const _: () = assert!(std::mem::size_of::<CtlPowerTelemetryV2>() == 1024);

// ---------------- 函数指针 ----------------

type FnInit = unsafe extern "system" fn(*mut CtlInitArgs, *mut *mut c_void) -> i32;
type FnEnumDevices = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type FnGetDeviceProps = unsafe extern "system" fn(*mut c_void, *mut CtlDeviceAdapterProperties) -> i32;
type FnEnumTemp = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type FnTempGetProps = unsafe extern "system" fn(*mut c_void, *mut CtlTempProperties) -> i32;
type FnTempGetState = unsafe extern "system" fn(*mut c_void, *mut f64) -> i32;
type FnEnumFans = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type FnFanGetProps = unsafe extern "system" fn(*mut c_void, *mut CtlFanProperties) -> i32;
type FnFanGetState = unsafe extern "system" fn(*mut c_void, i32, *mut i32) -> i32;
type FnPowerTelemetryGet = unsafe extern "system" fn(*mut c_void, *mut CtlPowerTelemetryV2) -> i32;
type FnEnumPowerDomains = unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
type FnPowerGetProps = unsafe extern "system" fn(*mut c_void, *mut CtlPowerProperties) -> i32;
type FnPowerGetEnergy = unsafe extern "system" fn(*mut c_void, *mut CtlPowerEnergyCounter) -> i32;

fn sensor_type_name(t: i32) -> &'static str {
    match t {
        0 => "GLOBAL(全设备最高)",
        1 => "GPU(芯片最高)",
        2 => "MEMORY(本地显存最高)",
        3 => "GLOBAL_MIN(全设备最低)",
        4 => "GPU_MIN",
        5 => "MEMORY_MIN",
        _ => "未知",
    }
}

fn item_f64(it: &CtlTelemetryItem) -> Option<f64> {
    if !it.supported {
        return None;
    }
    // ctl_data_type_t 的浮点:8=FLOAT, 9=DOUBLE
    let v = unsafe {
        match it.dtype {
            8 => it.value.datafloat as f64,
            9 => it.value.datadouble,
            0..=7 => it.value.datau64 as f64,
            _ => return None,
        }
    };
    Some(v)
}

fn print_item(label: &str, it: &CtlTelemetryItem) {
    match item_f64(it) {
        Some(v) => println!("    {label:<34} supported=true  type={} units={} value={v}", it.dtype, it.units),
        None => println!("    {label:<34} supported=false type={} units={}", it.dtype, it.units),
    }
}

fn main() {
    unsafe {
        let lib = match Library::new("ControlLib.dll") {
            Ok(l) => l,
            Err(e) => {
                println!("ControlLib.dll 加载失败: {e}");
                return;
            }
        };

        macro_rules! sym {
            ($name:literal, $ty:ty) => {
                match lib.get::<$ty>(concat!($name, "\0").as_bytes()) {
                    Ok(s) => *s,
                    Err(e) => {
                        println!("取符号 {} 失败: {e}", $name);
                        return;
                    }
                }
            };
        }
        let init: FnInit = sym!("ctlInit", FnInit);
        let enum_devices: FnEnumDevices = sym!("ctlEnumerateDevices", FnEnumDevices);
        let get_device_props: FnGetDeviceProps = sym!("ctlGetDeviceProperties", FnGetDeviceProps);
        let enum_temp: FnEnumTemp = sym!("ctlEnumTemperatureSensors", FnEnumTemp);
        let temp_get_props: FnTempGetProps = sym!("ctlTemperatureGetProperties", FnTempGetProps);
        let temp_get_state: FnTempGetState = sym!("ctlTemperatureGetState", FnTempGetState);
        let enum_fans: FnEnumFans = sym!("ctlEnumFans", FnEnumFans);
        let fan_get_props: FnFanGetProps = sym!("ctlFanGetProperties", FnFanGetProps);
        let fan_get_state: FnFanGetState = sym!("ctlFanGetState", FnFanGetState);
        let power_telemetry_v2: FnPowerTelemetryGet =
            sym!("ctlPowerTelemetryGetV2", FnPowerTelemetryGet);
        let enum_power_domains: FnEnumPowerDomains =
            sym!("ctlEnumPowerDomains", FnEnumPowerDomains);
        let power_get_props: FnPowerGetProps = sym!("ctlPowerGetProperties", FnPowerGetProps);
        let power_get_energy: FnPowerGetEnergy = sym!("ctlPowerGetEnergyCounter", FnPowerGetEnergy);

        let mut args = CtlInitArgs {
            size: std::mem::size_of::<CtlInitArgs>() as u32,
            version: 0,
            app_version: CTL_IMPL_VERSION,
            flags: CTL_INIT_FLAG_USE_LEVEL_ZERO,
            supported_version: 0,
            application_uid: [0; 16],
        };
        let mut api_handle: *mut c_void = std::ptr::null_mut();
        let r = init(&mut args, &mut api_handle);
        println!("ctlInit = 0x{r:08X} (handle={api_handle:p})");
        if (r != CTL_RESULT_SUCCESS && r != CTL_RESULT_SUCCESS_STILL_OPEN) || api_handle.is_null() {
            return;
        }

        let mut count: u32 = 0;
        let r = enum_devices(api_handle, &mut count, std::ptr::null_mut());
        println!("ctlEnumerateDevices(计数) = 0x{r:08X} count={count}");
        if count == 0 {
            return;
        }
        let count = (count as usize).min(MAX_DEVICES);
        let mut handles = vec![std::ptr::null_mut::<c_void>(); count];
        let mut count2 = count as u32;
        let r = enum_devices(api_handle, &mut count2, handles.as_mut_ptr());
        println!("ctlEnumerateDevices(句柄) = 0x{r:08X} count={count2}");

        for (i, &h) in handles.iter().enumerate() {
            if h.is_null() {
                continue;
            }
            let mut props: CtlDeviceAdapterProperties = std::mem::zeroed();
            props.size = std::mem::size_of::<CtlDeviceAdapterProperties>() as u32;
            props.version = 2;
            let r = get_device_props(h, &mut props);
            if r != CTL_RESULT_SUCCESS {
                println!("\n[{i}] ctlGetDeviceProperties 失败 0x{r:08X}");
                continue;
            }
            let name = String::from_utf8_lossy(&props.name)
                .trim_end_matches('\0')
                .to_string();
            println!(
                "\n[{i}] {name}  vendor={:04X} device={:04X} integrated={} type={} driver={}",
                props.pci_vendor_id,
                props.pci_device_id,
                props.graphics_adapter_properties & CTL_ADAPTER_PROPERTIES_FLAG_INTEGRATED != 0,
                props.device_type,
                props.driver_version
            );
            if props.device_type != CTL_DEVICE_TYPE_GRAPHICS {
                continue;
            }

            // ---- 通路 1:专用温度传感器枚举 ----
            println!("  -- ctlEnumTemperatureSensors --");
            let mut n: u32 = 0;
            let r = enum_temp(h, &mut n, std::ptr::null_mut());
            println!("    计数调用 = 0x{r:08X} 传感器数={n}");
            if r == CTL_RESULT_SUCCESS && n > 0 {
                let n = (n as usize).min(64);
                let mut th = vec![std::ptr::null_mut::<c_void>(); n];
                let mut n2 = n as u32;
                let r = enum_temp(h, &mut n2, th.as_mut_ptr());
                println!("    句柄调用 = 0x{r:08X} 传感器数={n2}");
                for (k, &t) in th.iter().enumerate() {
                    if t.is_null() {
                        continue;
                    }
                    let mut tp = CtlTempProperties {
                        size: std::mem::size_of::<CtlTempProperties>() as u32,
                        version: 1,
                        sensor_type: -1,
                        max_temperature: 0.0,
                    };
                    let rp = temp_get_props(t, &mut tp);
                    let mut temp: f64 = -999.0;
                    let rs = temp_get_state(t, &mut temp);
                    println!(
                        "    [{k}] GetProperties=0x{rp:08X} type={}({}) max={} | GetState=0x{rs:08X} temp={temp}",
                        tp.sensor_type,
                        sensor_type_name(tp.sensor_type),
                        tp.max_temperature
                    );
                }
            } else if r == CTL_RESULT_SUCCESS {
                println!("    -> 驱动报告 0 个温度传感器(该接口不被支持)");
            }

            // ---- 通路 2:风扇枚举 ----
            println!("  -- ctlEnumFans --");
            let mut fn_: u32 = 0;
            let r = enum_fans(h, &mut fn_, std::ptr::null_mut());
            println!("    计数调用 = 0x{r:08X} 风扇数={fn_}");
            if r == CTL_RESULT_SUCCESS && fn_ > 0 {
                let fn_ = (fn_ as usize).min(16);
                let mut fh = vec![std::ptr::null_mut::<c_void>(); fn_];
                let mut fn2 = fn_ as u32;
                let r = enum_fans(h, &mut fn2, fh.as_mut_ptr());
                println!("    句柄调用 = 0x{r:08X} 风扇数={fn2}");
                for (k, &f) in fh.iter().enumerate() {
                    if f.is_null() {
                        continue;
                    }
                    let mut fp = CtlFanProperties {
                        size: std::mem::size_of::<CtlFanProperties>() as u32,
                        version: 1,
                        ..std::mem::zeroed()
                    };
                    let rp = fan_get_props(f, &mut fp);
                    let mut rpm: i32 = -1;
                    let rr = fan_get_state(f, 0, &mut rpm); // 0 = CTL_FAN_SPEED_UNITS_RPM
                    let mut pct: i32 = -1;
                    let rpct = fan_get_state(f, 1, &mut pct); // 1 = PERCENT
                    println!(
                        "    [{k}] GetProperties=0x{rp:08X} canControl={} maxRPM={} modes=0x{:X} units=0x{:X} | GetState(RPM)=0x{rr:08X} {rpm} | (PCT)=0x{rpct:08X} {pct}",
                        fp.can_control, fp.max_rpm, fp.supported_modes, fp.supported_units
                    );
                }
            }

            // ---- 通路 3:功率域(核显上若给出 VRAM 域,即显存侧能耗) ----
            println!("  -- ctlEnumPowerDomains / ctlPowerGetEnergyCounter --");
            let mut pn: u32 = 0;
            let r = enum_power_domains(h, &mut pn, std::ptr::null_mut());
            println!("    计数调用 = 0x{r:08X} 功率域数={pn}");
            if r == CTL_RESULT_SUCCESS && pn > 0 {
                let pn = (pn as usize).min(16);
                let mut ph = vec![std::ptr::null_mut::<c_void>(); pn];
                let mut pn2 = pn as u32;
                let r = enum_power_domains(h, &mut pn2, ph.as_mut_ptr());
                println!("    句柄调用 = 0x{r:08X} 功率域数={pn2}");
                for (k, &p) in ph.iter().enumerate() {
                    if p.is_null() {
                        continue;
                    }
                    let mut pp = CtlPowerProperties {
                        size: std::mem::size_of::<CtlPowerProperties>() as u32,
                        version: 1,
                        ..std::mem::zeroed()
                    };
                    let rp = power_get_props(p, &mut pp);

                    let mut e1: CtlPowerEnergyCounter = std::mem::zeroed();
                    e1.size = std::mem::size_of::<CtlPowerEnergyCounter>() as u32;
                    e1.version = 1;
                    let r1 = power_get_energy(p, &mut e1);
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    let mut e2: CtlPowerEnergyCounter = std::mem::zeroed();
                    e2.size = std::mem::size_of::<CtlPowerEnergyCounter>() as u32;
                    e2.version = 1;
                    let r2 = power_get_energy(p, &mut e2);

                    // µJ / µs == W
                    let watts = if r1 == CTL_RESULT_SUCCESS
                        && r2 == CTL_RESULT_SUCCESS
                        && e2.timestamp_us > e1.timestamp_us
                    {
                        Some(
                            e2.energy_uj.saturating_sub(e1.energy_uj) as f64
                                / (e2.timestamp_us - e1.timestamp_us) as f64,
                        )
                    } else {
                        None
                    };
                    println!(
                        "    [{k}] GetProperties=0x{rp:08X} canControl={} default={}mW range={}..{}mW | 能量=0x{r1:08X}/0x{r2:08X} {}uJ -> {}",
                        pp.can_control,
                        pp.default_limit,
                        pp.min_limit,
                        pp.max_limit,
                        e2.energy_uj,
                        watts.map_or("功率不可算".to_string(), |w| format!("{w:.2} W"))
                    );
                }
            }

            // ---- 通路 4:V2 遥测的温度字段 ----
            println!("  -- ctlPowerTelemetryGetV2 --");
            let mut tel: CtlPowerTelemetryV2 = std::mem::zeroed();
            tel.size = 736; // sizeof(ctl_power_telemetry_v2_t)
            tel.version = 2;
            let r = power_telemetry_v2(h, &mut tel);
            println!("    调用 = 0x{r:08X}");
            print_item("gpuCurrentTemperature", &tel.gpu_current_temperature);
            print_item("gpuEnergyCounter", &tel.gpu_energy_counter);
            print_item("gpuCurrentClockFrequency", &tel.gpu_current_clock_frequency);
        }

        println!("\n---- 参考:hwprobe-core 的 V1 遥测能力集 ----");
        let caps = hwprobe_core::platform::windows::gpu_igcl::caps();
        println!("    V1 caps: {caps:?}");
        println!("    V1 读数: {:?}", hwprobe_core::platform::windows::gpu_igcl::read());
    }
}
