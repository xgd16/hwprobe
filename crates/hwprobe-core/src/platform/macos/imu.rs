//! IMU(惯性传感器)provider(macOS):Sudden Motion Sensor(突发运动传感器)。
//!
//! 2004-2010 年前后的 Intel MacBook / iBook 配有三轴加速度计(硬盘防跌落保护),
//! 经 IORegistry 的 `SMCMotionSensor` / `IOMotionSensor` / `AppleMotionSensor`
//! 服务暴露 user client。之后的机型(含全部 Apple Silicon)已移除该硬件,
//! 机器上没有任何运动传感器公开接口。
//!
//! 行为与 Windows 版(`platform/windows/imu.rs`)对齐:
//! - 机器无 IMU 时 `probe()` 返回 **Err**(类别整体不可用,Registry 记 warn 后
//!   跳过该类别),而不是空清单 —— 同 Windows 版"未发现加速度计/陀螺仪"的行为;
//! - 有设备才声明传感器,code 与 Windows 一致(`acceleration_{x,y,z}_g`);
//! - Sudden Motion Sensor 是纯三轴加速度计,**没有陀螺仪**:不声明
//!   `angular_velocity_*` 设备(Windows 侧同样是 Sensor API 里枚举到才声明),
//!   宁缺勿假;
//! - refresh 单源失败按传感器标 [`Status::Error`],绝不给假值。

#![cfg(target_os = "macos")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use io_kit_sys::types::io_connect_t;
use io_kit_sys::{IOConnectCallScalarMethod, IOServiceClose, IOServiceOpen};
use mach2::traps::mach_task_self;
use std::sync::Mutex;

use super::iokit::{self, IoObject};

/// 探测顺序:三者都是历史机型上的 Sudden Motion Sensor 驱动类名。
const MOTION_CLASSES: [&str; 3] = ["SMCMotionSensor", "IOMotionSensor", "AppleMotionSensor"];

/// 经典 Sudden Motion Sensor 的 user client 输出约定(unimotion / smsutil 等
/// 开源实现走的同一条通道):`IOConnectCallScalarMethod(selector 0)` 返回
/// 3 个标量 = X/Y/Z 原始计数,静止平放时约 256/g。
///
/// **本机(Apple Silicon)没有该硬件,此读取路径未经真机验证**;不同代际的
/// 比例因子可能不同,拿不准的值宁可失败:任何一步失败就整组标 `Error`,
/// 不写猜测值。
const SMS_SELECTOR: u32 = 0;
const SMS_COUNTS_PER_G: f64 = 256.0;

/// 一个已打开的运动传感器 user client(probe 创建,进程内保留)。
/// `_svc` 保活服务对象;`conn` 为 0 表示服务在但 user client 打不开,
/// refresh 时标 `Error`。
struct MotionSensor {
    conn: io_connect_t,
    _svc: IoObject,
}

// SAFETY: io_connect_t 是内核句柄,IOConnectCallScalarMethod 线程安全。
unsafe impl Send for MotionSensor {}
unsafe impl Sync for MotionSensor {}

impl Drop for MotionSensor {
    fn drop(&mut self) {
        if self.conn != 0 {
            // SAFETY: conn 来自 IOServiceOpen,只关闭一次。
            unsafe { IOServiceClose(self.conn) };
        }
    }
}

/// 在 IORegistry 里找运动传感器服务并打开 user client。
/// 服务存在即返回(服务对象, 连接句柄):user client 打不开时连接句柄为 0,
/// 设备照常声明,refresh 时标 `Error` —— 硬件在就不假装它不存在。
/// 服务不存在返回 None。
fn open_motion_sensor() -> Option<(IoObject, io_connect_t)> {
    for class in MOTION_CLASSES {
        let Some(svc) = iokit::matching_service(class) else {
            continue;
        };
        let mut conn: io_connect_t = 0;
        // SAFETY: svc 是有效 IOService;mach_task_self() 是本任务端口;
        // type 0 是此类驱动约定的连接类型;conn 出参由内核填写。
        let kr = unsafe { IOServiceOpen(svc.raw(), mach_task_self(), 0, &mut conn) };
        if kr != 0 {
            conn = 0;
        }
        return Some((svc, conn));
    }
    None
}

/// 读一次三轴原始计数(全有或全无,拿不到就是拿不到)。
fn read_raw(conn: io_connect_t) -> Option<[f64; 3]> {
    if conn == 0 {
        return None;
    }
    // SAFETY: output 缓冲与 outputCnt 匹配;失败时内核不写或只写部分,
    // 以回填数量为准判断。
    unsafe {
        let mut out = [0u64; 3];
        let mut n = 3u32;
        let kr = IOConnectCallScalarMethod(
            conn,
            SMS_SELECTOR,
            std::ptr::null(),
            0,
            out.as_mut_ptr(),
            &mut n,
        );
        if kr != 0 || n < 3 {
            return None;
        }
        let vals = [
            out[0] as f64 / SMS_COUNTS_PER_G,
            out[1] as f64 / SMS_COUNTS_PER_G,
            out[2] as f64 / SMS_COUNTS_PER_G,
        ];
        vals.iter().all(|v| v.is_finite()).then_some(vals)
    }
}

pub struct ImuProvider {
    sensors: Mutex<Vec<MotionSensor>>,
}

impl ImuProvider {
    pub fn new() -> ImuProvider {
        ImuProvider {
            sensors: Mutex::new(Vec::new()),
        }
    }
}

impl Default for ImuProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for ImuProvider {
    fn describe(&self) -> &str {
        "mac-imu"
    }

    fn category(&self) -> Category {
        Category::Imu
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        // 只找得到一个实例(机型上最多一颗 SMS)。服务不存在 → 同 Windows 版
        // "机器无 IMU" 行为:Err,类别整体不可用。
        let Some((svc, conn)) = open_motion_sensor() else {
            return Err(format!(
                "未发现运动传感器(IORegistry 无 {} 服务;Sudden Motion Sensor 只有 \
                 2004-2010 年的 Intel Mac 有,Apple Silicon 无 IMU)",
                MOTION_CLASSES.join("/")
            ));
        };

        // 加速度计(3 轴 g),code 与 Windows 版一致。
        let b = DeviceBuilder::new("imu:Accelerometer:0", "Accelerometer")
            .static_str("sensor_type", "Sensor Type", "Accelerometer")
            .dynamic("acceleration_x_g", "Acceleration X", "g", ValueType::F64)
            .dynamic("acceleration_y_g", "Acceleration Y", "g", ValueType::F64)
            .dynamic("acceleration_z_g", "Acceleration Z", "g", ValueType::F64)
            .build();
        // Sudden Motion Sensor 没有陀螺仪,不声明 angular_velocity_* 设备:
        // Windows 版同样是 Sensor API 里枚举到陀螺仪才声明,这里保持一致。

        let mut guard = self.sensors.lock().unwrap_or_else(|e| e.into_inner());
        guard.clear();
        guard.push(MotionSensor { conn, _svc: svc });

        Ok(vec![b])
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
            match read_raw(raw.conn) {
                Some([x, y, z]) => {
                    dev.set_reading("acceleration_x_g", Reading::now(Value::F64(x)));
                    dev.set_reading("acceleration_y_g", Reading::now(Value::F64(y)));
                    dev.set_reading("acceleration_z_g", Reading::now(Value::F64(z)));
                }
                None => {
                    // 单源失败:逐传感器标 Error,不给假值
                    for code in ["acceleration_x_g", "acceleration_y_g", "acceleration_z_g"] {
                        dev.set_reading(code, Reading::failed(Status::Error, ValueType::F64));
                    }
                }
            }
        }
    }
}
