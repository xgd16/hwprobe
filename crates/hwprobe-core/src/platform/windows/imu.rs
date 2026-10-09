//! IMU(惯性传感器)provider:经 Windows Sensor API(COM,用户态免管理员)
//! 读取加速度计与陀螺仪。掌机(如 MSI Claw A1M)的 IMU 经 Intel Sensor Hub
//! 以 HID Sensor 集合暴露,系统传感器类扩展已换算好单位:
//! 加速度 VT_R8(单位 g)、角速度 VT_R8(单位 °/s)。
//!
//! windows crate 未收录 sensorsapi 的 COM 接口,这里按 ABI 手写 vtable
//! (ISensorManager / ISensorCollection / ISensor / ISensorDataReport),
//! 方法顺序严格对应 SensorsApi.h 的 IDL 声明。GUID 取自 sensors.h。

#![cfg(target_os = "windows")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use std::sync::Mutex;
use windows::core::{GUID, IUnknown, Interface, BSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

// ---- 类/接口 GUID(SensorsApi.h)----
const CLSID_SENSOR_MANAGER: GUID =
    GUID::from_values(0x77A1_C827, 0xFCD2, 0x4689, [0x89, 0x15, 0x9D, 0x61, 0x3C, 0xC5, 0xFA, 0x3E]);
const IID_ISENSOR_MANAGER: GUID =
    GUID::from_values(0xBD77_DB67, 0x45A8, 0x42DC, [0x8D, 0x00, 0x6D, 0xCF, 0x15, 0xF8, 0x37, 0x7A]);

// ---- 传感器类型 GUID(sensors.h)----
const SENSOR_TYPE_ACCELEROMETER_3D: GUID =
    GUID::from_values(0xC2FB_0F5F, 0xE2D2, 0x4C78, [0xBC, 0xD0, 0x35, 0x2A, 0x95, 0x82, 0x81, 0x9D]);
const SENSOR_TYPE_GYROMETER_3D: GUID =
    GUID::from_values(0x0948_5F5A, 0x759E, 0x42C2, [0xBD, 0x4B, 0xA3, 0x49, 0xB7, 0x5C, 0x86, 0x43]);

// ---- 数据字段(sensors.h)----
/// SENSOR_DATA_TYPE_MOTION_GUID:pid 2..4 = 加速度 XYZ(g);pid 10..12 = 角速度 XYZ(°/s)。
const MOTION_DATA_GUID: GUID =
    GUID::from_values(0x3F8A_69A2, 0x07C5, 0x4E48, [0xA9, 0x65, 0xCD, 0x79, 0x7A, 0xAB, 0x56, 0xD5]);

fn motion_key(pid: u32) -> PROPERTYKEY {
    PROPERTYKEY {
        fmtid: MOTION_DATA_GUID,
        pid,
    }
}

// ---- 原始 PROPVARIANT(与 Windows ABI 一致,24 字节;只读 VT_R8)----
#[repr(C)]
union PvUnion {
    dbl: f64,
    _u64: u64,
}
#[repr(C)]
struct RawPropVariant {
    vt: u16,
    _r1: u16,
    _r2: u16,
    _r3: u16,
    u: PvUnion,
}

const VT_EMPTY: u16 = 0;
const VT_R8: u16 = 5;

impl RawPropVariant {
    fn zeroed() -> Self {
        RawPropVariant {
            vt: VT_EMPTY,
            _r1: 0,
            _r2: 0,
            _r3: 0,
            u: PvUnion { _u64: 0 },
        }
    }

    fn as_f64(&self) -> Option<f64> {
        if self.vt == VT_R8 {
            Some(unsafe { self.u.dbl })
        } else {
            None
        }
    }
}

// ---- COM vtable 定义(方法顺序 = IDL 声明顺序)----
type Hr = i32;
type Obj = *mut core::ffi::c_void;
type FnQueryInterface =
    unsafe extern "system" fn(Obj, *const GUID, *mut Obj) -> Hr;
type FnRelease = unsafe extern "system" fn(Obj) -> u32;

/// 槽序必须与 SensorsApi.h 的 ISensorManagerVtbl 一致:
/// 3 = GetSensorsByCategory,4 = GetSensorsByType(少一个占位就会把
/// 类型 GUID 当 category 传进 GetSensorsByCategory)。
#[repr(C)]
struct ISensorManagerVtbl {
    _query_interface: FnQueryInterface,
    _add_ref: FnRelease,
    release: FnRelease,
    _get_sensors_by_category: unsafe extern "system" fn(Obj, *const GUID, *mut Obj) -> Hr,
    get_sensors_by_type: unsafe extern "system" fn(Obj, *const GUID, *mut Obj) -> Hr,
}

#[repr(C)]
struct ISensorCollectionVtbl {
    _query_interface: FnQueryInterface,
    _add_ref: FnRelease,
    release: FnRelease,
    get_at: unsafe extern "system" fn(Obj, u32, *mut Obj) -> Hr,
    get_count: unsafe extern "system" fn(Obj, *mut u32) -> Hr,
}

/// ISensor 前段(至 GetData;后续事件相关方法不调用)。
/// 槽序 = ISensorVtbl:3..6 GetID/GetCategory/GetType/GetFriendlyName,
/// 7..10 GetProperty/GetProperties/GetSupportedDataFields/SetProperties,
/// 11 SupportsDataField,12 GetState,13 GetData。
#[repr(C)]
struct ISensorVtbl {
    _query_interface: FnQueryInterface,
    _add_ref: FnRelease,
    release: FnRelease,
    _get_id: unsafe extern "system" fn(Obj, *mut GUID) -> Hr,
    _get_category: unsafe extern "system" fn(Obj, *mut GUID) -> Hr,
    _get_type: unsafe extern "system" fn(Obj, *mut GUID) -> Hr,
    get_friendly_name: unsafe extern "system" fn(Obj, *mut *mut u16) -> Hr,
    _get_property: unsafe extern "system" fn(Obj, *const PROPERTYKEY, *mut RawPropVariant) -> Hr,
    _get_properties: unsafe extern "system" fn(Obj, *mut Obj) -> Hr,
    _get_supported_data_fields: unsafe extern "system" fn(Obj, *mut Obj) -> Hr,
    _set_properties: unsafe extern "system" fn(Obj, Obj, *mut Obj) -> Hr,
    _supports_data_field: unsafe extern "system" fn(Obj, *const PROPERTYKEY, *mut i16) -> Hr,
    _get_state: unsafe extern "system" fn(Obj, *mut i32) -> Hr,
    get_data: unsafe extern "system" fn(Obj, *mut Obj) -> Hr,
}

#[repr(C)]
struct ISensorDataReportVtbl {
    _query_interface: FnQueryInterface,
    _add_ref: FnRelease,
    release: FnRelease,
    _get_timestamp: unsafe extern "system" fn(Obj, *mut core::ffi::c_void) -> Hr,
    get_sensor_value: unsafe extern "system" fn(Obj, *const PROPERTYKEY, *mut RawPropVariant) -> Hr,
}

/// vtable 取用:COM 对象指针首 8 字节为 vtable 指针。
fn vtbl_of<T>(obj: Obj) -> *mut T {
    unsafe { *(obj as *mut *mut T) }
}

/// 一个已打开的原始传感器 COM 对象(probe 创建,进程生命周期内保留)。
struct RawSensor {
    obj: Obj,
    is_gyro: bool,
}

unsafe impl Send for RawSensor {}
unsafe impl Sync for RawSensor {}

pub struct ImuProvider {
    sensors: Mutex<Vec<RawSensor>>,
}

impl ImuProvider {
    pub fn new() -> ImuProvider {
        ImuProvider {
            sensors: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for ImuProvider {
    fn describe(&self) -> &str {
        "win-imu"
    }

    fn category(&self) -> Category {
        Category::Imu
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        // 确保本线程处于 MTA(前面 provider 已初始化过,幂等)
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        // CoCreateInstance 只能给 windows crate 认识的接口类型;先取 IUnknown
        // 再手工 QI 到 ISensorManager(返回的就是该接口指针)。
        let manager: Obj = unsafe {
            let unk: IUnknown = CoCreateInstance(
                &CLSID_SENSOR_MANAGER,
                None,
                CLSCTX_INPROC_SERVER,
            )
            .map_err(|e| format!("SensorManager COM 初始化失败: {e}"))?;
            let raw = unk.as_raw();
            qi(raw, &IID_ISENSOR_MANAGER).ok_or("ISensorManager QI 失败")?
        };

        let mut sensors = Vec::new();
        let mut devices = Vec::new();

        for (type_guid, is_gyro, label, axes_meta) in [
            (
                &SENSOR_TYPE_ACCELEROMETER_3D,
                false,
                "Accelerometer",
                (
                    ["acceleration_x_g", "acceleration_y_g", "acceleration_z_g"],
                    ["Acceleration X", "Acceleration Y", "Acceleration Z"],
                    "g",
                ),
            ),
            (
                &SENSOR_TYPE_GYROMETER_3D,
                true,
                "Gyroscope",
                (
                    [
                        "angular_velocity_x_dps",
                        "angular_velocity_y_dps",
                        "angular_velocity_z_dps",
                    ],
                    ["Angular Velocity X", "Angular Velocity Y", "Angular Velocity Z"],
                    "°/s",
                ),
            ),
        ] {
            let mut collection: Obj = std::ptr::null_mut();
            let hr = unsafe {
                ((*vtbl_of::<ISensorManagerVtbl>(manager)).get_sensors_by_type)(manager, type_guid, &mut collection)
            };
            if hr < 0 || collection.is_null() {
                continue;
            }

            let mut count = 0u32;
            let hr =
                unsafe { ((*vtbl_of::<ISensorCollectionVtbl>(collection)).get_count)(collection, &mut count) };
            if hr < 0 {
                unsafe { release(collection) };
                continue;
            }

            for i in 0..count {
                let mut sensor: Obj = std::ptr::null_mut();
                let hr =
                    unsafe { ((*vtbl_of::<ISensorCollectionVtbl>(collection)).get_at)(collection, i, &mut sensor) };
                if hr < 0 || sensor.is_null() {
                    continue;
                }

                let (codes, names, unit) = axes_meta;
                let name = unsafe { get_friendly_name(sensor) }.unwrap_or_else(|| label.to_string());

                let mut b = DeviceBuilder::new(format!("imu:{label}:{i}"), name.clone())
                    .static_str("sensor_type", "Sensor Type", label);
                for (code, disp) in codes.into_iter().zip(names) {
                    b = b.dynamic(code, disp, unit, ValueType::F64);
                }
                devices.push(b.build());
                sensors.push(RawSensor { obj: sensor, is_gyro });
            }

            unsafe { release(collection) };
        }

        unsafe { release(manager) };

        if sensors.is_empty() {
            return Err("未发现加速度计/陀螺仪(Windows Sensor API 无 3D 运动传感器)".into());
        }

        *self.sensors.lock().unwrap_or_else(|e| e.into_inner()) = sensors;
        Ok(devices)
    }

    fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_millis(200)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let sensors = self.sensors.lock().unwrap_or_else(|e| e.into_inner());
        for (k, raw) in sensors.iter().enumerate() {
            let Some(dev) = devices.get_mut(k) else {
                return;
            };
            refresh_one(raw, dev);
        }
    }
}

fn refresh_one(raw: &RawSensor, dev: &mut DeviceData) {
    let mut report: Obj = std::ptr::null_mut();
    let hr = unsafe { ((*vtbl_of::<ISensorVtbl>(raw.obj)).get_data)(raw.obj, &mut report) };
    if hr < 0 || report.is_null() {
        for s in dev.sensors.iter_mut() {
            if s.desc.kind == crate::model::SensorKind::Dynamic {
                s.current = Reading::failed(Status::Error, ValueType::F64);
            }
        }
        return;
    }

    // 加速度 pid 2..4(g);角速度 pid 10..12(°/s)
    let pids: [u32; 3] = if raw.is_gyro { [10, 11, 12] } else { [2, 3, 4] };
    let codes: [&str; 3] = if raw.is_gyro {
        [
            "angular_velocity_x_dps",
            "angular_velocity_y_dps",
            "angular_velocity_z_dps",
        ]
    } else {
        ["acceleration_x_g", "acceleration_y_g", "acceleration_z_g"]
    };

    for (code, pid) in codes.into_iter().zip(pids) {
        let mut pv = RawPropVariant::zeroed();
        let hr = unsafe {
            ((*vtbl_of::<ISensorDataReportVtbl>(report)).get_sensor_value)(
                report,
                &motion_key(pid),
                &mut pv,
            )
        };
        match (hr >= 0).then(|| pv.as_f64()).flatten() {
            Some(v) => dev.set_reading(code, Reading::now(Value::F64(v))),
            None => dev.set_reading(code, Reading::failed(Status::Error, ValueType::F64)),
        }
    }

    unsafe { release(report) };
}

/// 手工 QueryInterface(成功返回新引用,调用方负责 Release)。
unsafe fn qi(obj: Obj, iid: &GUID) -> Option<Obj> {
    let qi_fn = unsafe { (*vtbl_of::<ISensorManagerVtbl>(obj))._query_interface };
    let mut out: Obj = std::ptr::null_mut();
    (qi_fn(obj, iid, &mut out) >= 0).then_some(out)
}

/// 手工 IUnknown::Release。
unsafe fn release(obj: Obj) {
    if !obj.is_null() {
        let release_fn = unsafe { (*vtbl_of::<ISensorManagerVtbl>(obj)).release };
        release_fn(obj);
    }
}

/// GetFriendlyName:返回 BSTR(SensorsApi.h 声明为 BSTR*)。BSTR 指针指向
/// 4 字节长度前缀之后的字符数据,不是分配块基址 —— 必须用 SysFreeString
/// 释放(交给 `windows::core::BSTR` 的 Drop),绝不能交给 CoTaskMemFree,
/// 否则堆管理器按块基址解析会判定堆损坏(0xC0000374)。
unsafe fn get_friendly_name(sensor: Obj) -> Option<String> {
    let mut ptr: *mut u16 = std::ptr::null_mut();
    let hr = unsafe { ((*vtbl_of::<ISensorVtbl>(sensor)).get_friendly_name)(sensor, &mut ptr) };
    if hr < 0 || ptr.is_null() {
        return None;
    }
    let s = unsafe { BSTR::from_raw(ptr) }.to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// vtable 槽位必须与 SensorsApi.h 的 IDL 声明顺序一致(偏移 = 槽号 × 指针宽)。
    /// 少写一个占位字段就会整体错位:把 `GetSensorsByType` 调成
    /// `GetSensorsByCategory`,把 `GetData` 调成 `GetState`(后者会把状态值
    /// 当接口指针解引用)。
    #[test]
    fn com_vtable_slots() {
        let p = std::mem::size_of::<usize>();
        assert_eq!(std::mem::offset_of!(ISensorManagerVtbl, release), 2 * p);
        assert_eq!(
            std::mem::offset_of!(ISensorManagerVtbl, _get_sensors_by_category),
            3 * p
        );
        assert_eq!(
            std::mem::offset_of!(ISensorManagerVtbl, get_sensors_by_type),
            4 * p
        );

        assert_eq!(std::mem::offset_of!(ISensorCollectionVtbl, get_at), 3 * p);
        assert_eq!(std::mem::offset_of!(ISensorCollectionVtbl, get_count), 4 * p);

        assert_eq!(std::mem::offset_of!(ISensorVtbl, get_friendly_name), 6 * p);
        assert_eq!(std::mem::offset_of!(ISensorVtbl, _supports_data_field), 11 * p);
        assert_eq!(std::mem::offset_of!(ISensorVtbl, _get_state), 12 * p);
        assert_eq!(std::mem::offset_of!(ISensorVtbl, get_data), 13 * p);

        assert_eq!(
            std::mem::offset_of!(ISensorDataReportVtbl, get_sensor_value),
            4 * p
        );
    }
}
