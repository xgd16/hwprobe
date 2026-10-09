//! hwprobe.dll 的 C ABI 导出层。
//!
//! 契约要点:
//! - 所有导出 `extern "system"`,入口处 catch_unwind,绝不跨 FFI panic;
//! - 字符串一律 UTF-8,固定长度缓冲区(结构体内)或 caller 缓冲区;
//! - 返回值为 i32 状态码(见 hwprobe.h / model::Status);
//! - 读操作仅持读锁,最长等待一次轻量刷新(毫秒级)。

#![crate_name = "hwprobe"]

use hwprobe_core::logbuf;
use hwprobe_core::model::{
    Category, DeviceData, Reading, Status, Value, ValueType, ALL_CATEGORIES,
};
use hwprobe_core::stream::StreamBatch;
use hwprobe_core::{PlatformBackend, Registry};
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// ABI 版本,由 core 单点定义后在此转出(见 `hwprobe_core::HWPROBE_ABI_VERSION`)。
/// 保留这个名字是因为它是 dll crate 的公开常量,`hwprobe_abi_version()` 直接回报它。
pub use hwprobe_core::HWPROBE_ABI_VERSION;

// ---------------- C 结构体(与 include/hwprobe.h 严格对应) ----------------

#[repr(C)]
pub struct FfiCategoryInfo {
    pub size: u32,
    pub index: u32,
    pub name: [u8; 32],
    pub device_count: u32,
}

#[repr(C)]
pub struct FfiDeviceInfo {
    pub size: u32,
    pub category: u32,
    pub index: u32,
    pub id: [u8; 128],
    pub name: [u8; 192],
    pub vendor: [u8; 96],
    pub model: [u8; 96],
    pub serial: [u8; 96],
}

#[repr(C)]
pub struct FfiSensorInfo {
    pub size: u32,
    pub code: [u8; 64],
    pub name: [u8; 96],
    pub unit: [u8; 16],
    pub kind: u32,
    pub value_type: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FfiValueUnion {
    pub f64_: f64,
    pub u64_: u64,
    pub i64_: i64,
    pub b: u8,
    pub s: [u8; 96],
}

#[repr(C)]
pub struct FfiReading {
    pub size: u32,
    pub value_type: u32,
    pub status: u32,
    pub timestamp_ms: u64,
    pub value: FfiValueUnion,
}

/// 订阅推送的单条样本(与 hwprobe_stream_item 对应)。
#[repr(C)]
pub struct FfiStreamItem {
    pub size: u32,
    pub category: u32,
    pub device: u32,
    pub sensor: u32,
    pub value_type: u32,
    pub status: u32,
    pub timestamp_ms: u64,
    pub value: FfiValueUnion,
}

/// 订阅推送的一批样本(与 hwprobe_stream_batch 对应)。
/// items 指针仅在本次回调执行期间有效,回调返回即失效。
#[repr(C)]
pub struct FfiStreamBatch {
    pub size: u32,
    pub count: u32,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub items: *const FfiStreamItem,
}

/// 订阅回调原型:在 DLL 内部轮询线程上被调用。
pub type HwprobeStreamCallback = extern "system" fn(
    batch: *const FfiStreamBatch,
    user_data: *mut c_void,
);

// ---------------- 全局状态 ----------------

struct App {
    registry: Arc<Registry>,
    backend: RwLock<Option<Box<dyn PlatformBackend>>>,
}

static APP: OnceLock<App> = OnceLock::new();

/// 初始化串行锁:头文件承诺导出可并发调用,并发 init 时败者在锁上等待,
/// 随后走幂等路径 —— 否则双方都会 build_on_worker + spawn_poller,
/// OnceLock 只接纳胜者,败者的 poller 线程永久泄漏。
static INIT_LOCK: Mutex<()> = Mutex::new(());

fn app() -> Result<&'static App, Status> {
    APP.get().ok_or(Status::NotInitialized)
}

/// FFI 安全包装:捕获 panic,统一记录 last_error。
fn guard(f: impl FnOnce() -> Result<i32, Status>) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(code)) => code,
        Ok(Err(status)) => status as i32,
        Err(_) => {
            logbuf::error("FFI 边界捕获到 panic");
            Status::Unknown as i32
        }
    }
}

fn do_init() -> Result<i32, Status> {
    let _g = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(app) = APP.get() {
        // shutdown 后再次 init:恢复后台轮询与后端(Registry 与设备缓存仍在,
        // 无需重新 probe)
        app.registry.spawn_poller();
        let mut b = app.backend.write().unwrap_or_else(|e| e.into_inner());
        if b.is_none() {
            let backend = hwprobe_core::create_backend().map_err(|_| Status::InitFailed)?;
            let _ = backend.init();
            *b = Some(backend);
        }
        return Ok(Status::Ok as i32);
    }
    // 在专职线程上创建 + probe(宿主线程可能是 STA,WMI 需要 MTA)
    let (backend, registry) =
        hwprobe_core::registry::build_on_worker().map_err(|e| {
            logbuf::error(format!("初始化失败: {e}"));
            Status::InitFailed
        })?;
    registry.spawn_poller();
    let _ = APP.set(App {
        registry,
        backend: RwLock::new(Some(backend)),
    });
    Ok(Status::Ok as i32)
}

fn do_shutdown() -> Result<i32, Status> {
    if let Some(app) = APP.get() {
        app.registry.stop_poller();
        if let Some(b) = app.backend.write().unwrap().take() {
            b.shutdown();
        }
    }
    Ok(Status::Ok as i32)
}

fn registry() -> Result<&'static Arc<Registry>, Status> {
    app().map(|a| &a.registry)
}

// ---------------- 导出 ----------------

#[no_mangle]
pub extern "system" fn hwprobe_abi_version() -> u32 {
    HWPROBE_ABI_VERSION
}

/// 初始化(枚举硬件 + 启动后台轮询)。幂等。
#[no_mangle]
pub extern "system" fn hwprobe_init() -> i32 {
    guard(do_init)
}

/// 关闭(停止轮询线程)。幂等。
#[no_mangle]
pub extern "system" fn hwprobe_shutdown() -> i32 {
    guard(do_shutdown)
}

/// 设置轮询间隔(100~60000ms)。
#[no_mangle]
pub extern "system" fn hwprobe_set_poll_interval(ms: u32) -> i32 {
    guard(|| {
        let r = registry()?;
        r.set_poll_interval(ms);
        Ok(Status::Ok as i32)
    })
}

/// 设置占用口径:0 = 标准(时间差分 / 厂商 API),1 = 类 Windows 任务管理器
/// (PDH % Processor Utility / GPU Engine 引擎计数器)。可在 init 前调用,
/// 下一次刷新生效,运行中可随时切换;未知取值返回 HWPROBE_INVALID_ARG。
#[no_mangle]
pub extern "system" fn hwprobe_set_usage_mode(mode: u32) -> i32 {
    guard(|| {
        if hwprobe_core::options::set_usage_mode(mode) {
            Ok(Status::Ok as i32)
        } else {
            Err(Status::InvalidArg)
        }
    })
}

#[no_mangle]
pub extern "system" fn hwprobe_get_category_count(out: *mut u32) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        unsafe { *out = hwprobe_core::model::CATEGORY_COUNT };
        Ok(Status::Ok as i32)
    })
}

#[no_mangle]
pub extern "system" fn hwprobe_get_category_info(index: u32, out: *mut FfiCategoryInfo) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        let cat = Category::from_u32(index).ok_or(Status::BadCategory)?;
        let count = registry()?.device_count(index).unwrap_or(0);
        unsafe {
            let o = &mut *out;
            o.size = std::mem::size_of::<FfiCategoryInfo>() as u32;
            o.index = index;
            o.device_count = count;
            hwprobe_core::util::copy_to_fixed(&mut o.name, cat.name());
        }
        Ok(Status::Ok as i32)
    })
}

#[no_mangle]
pub extern "system" fn hwprobe_get_device_count(category: u32, out: *mut u32) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        let count = registry()?.device_count(category)?;
        unsafe { *out = count };
        Ok(Status::Ok as i32)
    })
}

fn fill_device(dst: &mut FfiDeviceInfo, category: u32, index: u32, dev: &DeviceData) {
    dst.size = std::mem::size_of::<FfiDeviceInfo>() as u32;
    dst.category = category;
    dst.index = index;
    hwprobe_core::util::copy_to_fixed(&mut dst.id, &dev.meta.id);
    hwprobe_core::util::copy_to_fixed(&mut dst.name, &dev.meta.name);
    hwprobe_core::util::copy_to_fixed(&mut dst.vendor, &dev.meta.vendor);
    hwprobe_core::util::copy_to_fixed(&mut dst.model, &dev.meta.model);
    hwprobe_core::util::copy_to_fixed(&mut dst.serial, &dev.meta.serial);
}

/// 把 Rust 值写入 96 字节重叠 union(仅 value_type 对应的成员有效)。
fn fill_value_union(value: &Value) -> FfiValueUnion {
    let mut u = FfiValueUnion { f64_: 0.0 };
    match value {
        Value::F64(v) => u.f64_ = *v,
        Value::U64(v) => u.u64_ = *v,
        Value::I64(v) => u.i64_ = *v,
        Value::Bool(v) => u.b = *v as u8,
        Value::Str(v) => {
            let mut tmp = [0u8; 96];
            hwprobe_core::util::copy_to_fixed(&mut tmp, v);
            u.s = tmp;
        }
    }
    u
}

#[no_mangle]
pub extern "system" fn hwprobe_get_device_info(category: u32, device: u32, out: *mut FfiDeviceInfo) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        let dev = registry()?.device(category, device)?;
        unsafe {
            fill_device(&mut *out, category, device, &dev);
        }
        Ok(Status::Ok as i32)
    })
}

#[no_mangle]
pub extern "system" fn hwprobe_get_sensor_count(category: u32, device: u32, out: *mut u32) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        let count = registry()?.sensor_count(category, device)?;
        unsafe { *out = count };
        Ok(Status::Ok as i32)
    })
}

#[no_mangle]
pub extern "system" fn hwprobe_get_sensor_info(
    category: u32,
    device: u32,
    sensor: u32,
    out: *mut FfiSensorInfo,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        let dev = registry()?.device(category, device)?;
        let sen = dev.sensors.get(sensor as usize).ok_or(Status::BadSensor)?;
        unsafe {
            let o = &mut *out;
            o.size = std::mem::size_of::<FfiSensorInfo>() as u32;
            hwprobe_core::util::copy_to_fixed(&mut o.code, &sen.desc.code);
            hwprobe_core::util::copy_to_fixed(&mut o.name, &sen.desc.name);
            hwprobe_core::util::copy_to_fixed(&mut o.unit, &sen.desc.unit);
            o.kind = sen.desc.kind as u32;
            o.value_type = sen.desc.vtype as u32;
        }
        Ok(Status::Ok as i32)
    })
}

/// 立即刷新一个类别(ALL_CATEGORIES = 全部)。通常不需要调用,后台自动轮询。
#[no_mangle]
pub extern "system" fn hwprobe_refresh(category: u32) -> i32 {
    guard(|| {
        let r = registry()?;
        if category == ALL_CATEGORIES {
            r.refresh(None);
        } else {
            let cat = Category::from_u32(category).ok_or(Status::BadCategory)?;
            r.refresh(Some(cat));
        }
        Ok(Status::Ok as i32)
    })
}

#[no_mangle]
pub extern "system" fn hwprobe_read_sensor(
    category: u32,
    device: u32,
    sensor: u32,
    out: *mut FfiReading,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(Status::InvalidArg);
        }
        let reading = registry()?.read(category, device, sensor)?;
        unsafe {
            let o = &mut *out;
            o.size = std::mem::size_of::<FfiReading>() as u32;
            o.status = reading.status as u32;
            o.value_type = reading.value.type_tag() as u32;
            o.timestamp_ms = reading.timestamp_ms;
            o.value = fill_value_union(&reading.value);
            let _ = ValueType::F64; // keep import
        }
        Ok(Status::Ok as i32)
    })
}

/// JSON 便捷导出。buf 为 NULL 时 *len 返回所需大小(含 NUL)。
#[no_mangle]
pub extern "system" fn hwprobe_dump_json(category: u32, buf: *mut u8, len: *mut u32) -> i32 {
    guard(|| {
        if len.is_null() {
            return Err(Status::InvalidArg);
        }
        let r = registry()?;
        let json = r.dump_json(category).to_string();
        let needed = json.len() as u32 + 1;
        if buf.is_null() {
            unsafe { *len = needed };
            return Ok(Status::Ok as i32);
        }
        let cap = unsafe { *len };
        if cap < needed {
            unsafe { *len = needed };
            return Err(Status::BufferTooSmall);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(json.as_ptr(), buf, json.len());
            *buf.add(json.len()) = 0;
            *len = needed;
        }
        Ok(Status::Ok as i32)
    })
}

/// 最近一条错误信息。buf 为 NULL 时 *len 返回所需大小(含 NUL)。
#[no_mangle]
pub extern "system" fn hwprobe_last_error(buf: *mut u8, len: *mut u32) -> i32 {
    guard(|| {
        if len.is_null() {
            return Err(Status::InvalidArg);
        }
        let msg = logbuf::global()
            .last_error()
            .unwrap_or_else(|| "no error".into());
        let needed = msg.len() as u32 + 1;
        if buf.is_null() {
            unsafe { *len = needed };
            return Ok(Status::Ok as i32);
        }
        let cap = unsafe { *len };
        if cap < needed {
            unsafe { *len = needed };
            return Err(Status::BufferTooSmall);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(msg.as_ptr(), buf, msg.len());
            *buf.add(msg.len()) = 0;
            *len = needed;
        }
        Ok(Status::Ok as i32)
    })
}

// ---------------- 订阅式流式推送 ----------------

/// 把一批 Rust 快照编组为 C 结构并调用外部回调。
/// items 缓冲区临时构建,仅在本次调用期间有效。
fn marshal_and_call(batch: &StreamBatch, cb: HwprobeStreamCallback, user: usize) {
    let items: Vec<FfiStreamItem> = batch
        .items
        .iter()
        .map(|it| FfiStreamItem {
            size: std::mem::size_of::<FfiStreamItem>() as u32,
            category: it.category,
            device: it.device,
            sensor: it.sensor,
            value_type: it.value_type,
            status: it.status,
            timestamp_ms: it.timestamp_ms,
            value: fill_value_union(&it.value),
        })
        .collect();
    let fb = FfiStreamBatch {
        size: std::mem::size_of::<FfiStreamBatch>() as u32,
        count: items.len() as u32,
        seq: batch.seq,
        timestamp_ms: batch.timestamp_ms,
        items: items.as_ptr(),
    };
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cb(&fb, user as *mut c_void)
    }));
}

/// 订阅动态数据流:注册回调后,DLL 在内部轮询线程上按节拍把全部动态
/// 传感器的快照推给回调,调用方无需再自行轮询读值/刷新。
///
/// 参数:
/// - `callback`:回调原型 hwprobe_stream_cb,不可为 NULL;
/// - `user_data`:原样回传给回调的上下文指针,可为 NULL;
/// - `interval_ms`:期望推送间隔,0 = 跟随后台节拍;非零会把后台节拍
///   自动调低到不超过该间隔(下限 100ms);实际推送频率还受各数据源
///   TTL 限制(数据没更新时推送相同值,seq 仍递增);
/// - `category_mask`:类别掩码,bit i = 类别 i;
///   HWPROBE_ALL_CATEGORIES(0xFFFFFFFF)= 全部,0 非法;
/// - `out_handle`:接收订阅句柄,用于 hwprobe_unsubscribe。
///
/// 线程契约:回调在 DLL 内部线程执行,必须快速返回(只做拷贝/入队),
/// 不要在回调中调用 hwprobe_refresh / hwprobe_shutdown / 阻塞操作;
/// 允许在回调内 hwprobe_unsubscribe 自身句柄。items 指针仅在本次回调
/// 执行期间有效。须先 hwprobe_init()。
#[no_mangle]
pub extern "system" fn hwprobe_subscribe(
    callback: Option<HwprobeStreamCallback>,
    user_data: *mut c_void,
    interval_ms: u32,
    category_mask: u32,
    out_handle: *mut u64,
) -> i32 {
    guard(|| {
        let cb = callback.ok_or(Status::InvalidArg)?;
        if out_handle.is_null() || category_mask == 0 {
            return Err(Status::InvalidArg);
        }
        let registry = registry()?.clone();
        let ud = user_data as usize; // 裸指针以 usize 携带,便于跨线程闭包捕获
        let handler: hwprobe_core::stream::StreamCallback = Arc::new(move |batch| {
            marshal_and_call(batch, cb, ud);
        });
        let handle = registry.subscribe(interval_ms, category_mask, handler);
        unsafe { *out_handle = handle };
        Ok(Status::Ok as i32)
    })
}

/// 取消订阅。返回后该订阅的回调保证不会再被调用;
/// 句柄不存在(或已取消)返回 HWPROBE_BAD_HANDLE。
#[no_mangle]
pub extern "system" fn hwprobe_unsubscribe(handle: u64) -> i32 {
    guard(|| {
        registry()?.unsubscribe(handle)?;
        Ok(Status::Ok as i32)
    })
}

/// 一键安装 PawnIO 驱动增强层:运行内置的官方签名安装器,弹出 UAC 后
/// 静默安装。幂等,已安装时直接返回 OK。此为显式操作,hwprobe_init 不会
/// 自动调用;安装成功后本进程内已缓存的驱动初始化结果不会重试,需重启
/// 进程生效,且访问 PawnIO 设备仍需管理员权限。失败原因经
/// hwprobe_last_error 获取。
#[no_mangle]
pub extern "system" fn hwprobe_setup_driver() -> i32 {
    guard(|| {
        #[cfg(target_os = "windows")]
        let result = {
            let installed =
                hwprobe_core::platform::windows::driver::install_pawnio().map_err(|e| {
                    logbuf::error(format!("PawnIO 安装失败: {e}"));
                    Status::Error
                });
            if installed.is_ok() && APP.get().is_some() {
                logbuf::warn("PawnIO 刚完成安装;本进程驱动初始化结果已缓存,重启进程后生效");
            }
            installed.map(|_| Status::Ok as i32)
        };
        #[cfg(not(target_os = "windows"))]
        let result = Err(Status::NotSupported);
        result
    })
}

// 引用 Reading 以防 unused 告警(read_sensor 使用)
#[allow(dead_code)]
fn _type_check(_: Reading) {}
