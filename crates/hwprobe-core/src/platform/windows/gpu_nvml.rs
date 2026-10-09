//! NVIDIA NVML 运行时加载(libloading),零链接期依赖。
//!
//! 不用 nvml-wrapper 等绑定 crate 的原因:它们在编译期链接 nvml.lib,
//! 没装 NVIDIA 驱动的机器上 LoadLibrary(hwprobe.dll) 会直接失败。
//! 这里手动绑定最少的 ~15 个入口,加载失败优雅返回 None。

#![cfg(target_os = "windows")]

use libloading::{Library, Symbol};
use std::sync::OnceLock;

pub const NVML_TEMPERATURE_GPU: u32 = 0;
pub const NVML_CLOCK_GRAPHICS: u32 = 0;
pub const NVML_CLOCK_MEM: u32 = 2;

#[repr(C)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[repr(C)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

pub struct Nvml {
    lib: Library,
}

unsafe impl Send for Nvml {}
unsafe impl Sync for Nvml {}

type FnInitV2 = unsafe extern "C" fn() -> i32;
type FnShutdown = unsafe extern "C" fn() -> i32;
type FnGetCount = unsafe extern "C" fn(*mut u32) -> i32;
type FnGetHandle = unsafe extern "C" fn(u32, *mut isize) -> i32;
type FnGetName = unsafe extern "C" fn(isize, *mut u8, u32) -> i32;
type FnGetTemp = unsafe extern "C" fn(isize, u32, *mut u32) -> i32;
/// nvmlDeviceGetFanSpeed 是 2 参的 v1 签名(device, *speed),全驱动版本可用;
/// 三参的按 fan 索引查询是另一个符号 nvmlDeviceGetFanSpeed_v2,不能混绑。
type FnGetFanSpeed = unsafe extern "C" fn(isize, *mut u32) -> i32;
type FnGetPowerUsage = unsafe extern "C" fn(isize, *mut u32) -> i32;
type FnGetPowerLimit = unsafe extern "C" fn(isize, *mut u32) -> i32;
type FnGetUtil = unsafe extern "C" fn(isize, *mut NvmlUtilization) -> i32;
type FnGetMem = unsafe extern "C" fn(isize, *mut NvmlMemory) -> i32;
/// nvmlDeviceGetClockInfo / GetMaxClockInfo 均为 (device, clockType, *clock),
/// 返回 nvmlReturn_t 状态码;频率值经输出指针带回。
type FnGetClock = unsafe extern "C" fn(isize, u32, *mut u32) -> i32;
type FnGetMaxClock = unsafe extern "C" fn(isize, u32, *mut u32) -> i32;

static NVML: OnceLock<Option<Nvml>> = OnceLock::new();

/// 尝试加载 nvml.dll(系统目录或旧版路径)。失败返回 None,不影响其他功能。
pub fn try_load() -> Option<&'static Nvml> {
    NVML.get_or_init(|| {
        let candidates = [
            "C:\\Windows\\System32\\nvml.dll",
            "C:\\Program Files\\NVIDIA Corporation\\NVSMI\\nvml.dll",
        ];
        for path in candidates {
            let Ok(lib) = (unsafe { Library::new(path) }) else {
                continue;
            };
            let nvml = Nvml { lib };
            // init 验证
            let ok = unsafe {
                let f: Symbol<FnInitV2> = nvml.lib.get(b"nvmlInit_v2\0").ok()?;
                f() == 0
            };
            if ok {
                crate::logbuf::info(format!("NVML 加载成功: {path}"));
                return Some(nvml);
            }
            crate::logbuf::warn("NVML 加载成功但 nvmlInit_v2 失败");
            return None;
        }
        None
    })
    .as_ref()
}

impl Nvml {
    fn sym<T>(&self, name: &[u8]) -> Option<Symbol<T>> {
        unsafe { self.lib.get(name).ok() }
    }

    pub fn device_count(&self) -> u32 {
        unsafe {
            match self.sym::<FnGetCount>(b"nvmlDeviceGetCount_v2\0") {
                Some(f) => {
                    let mut n = 0u32;
                    if f(&mut n) == 0 {
                        n
                    } else {
                        0
                    }
                }
                None => 0,
            }
        }
    }

    pub fn device_handle(&self, index: u32) -> Option<isize> {
        unsafe {
            let f: Symbol<FnGetHandle> = self.sym(b"nvmlDeviceGetHandleByIndex_v2\0")?;
            let mut h = 0isize;
            if f(index, &mut h) == 0 {
                Some(h)
            } else {
                None
            }
        }
    }

    pub fn device_name(&self, h: isize) -> String {
        unsafe {
            let Some(f) = self.sym::<FnGetName>(b"nvmlDeviceGetName\0") else {
                return String::new();
            };
            let mut buf = [0u8; 96];
            if f(h, buf.as_mut_ptr(), buf.len() as u32) == 0 {
                let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..end]).to_string()
            } else {
                String::new()
            }
        }
    }

    pub fn temperature(&self, h: isize) -> Option<f64> {
        unsafe {
            let f: Symbol<FnGetTemp> = self.sym(b"nvmlDeviceGetTemperature\0")?;
            let mut t = 0u32;
            ((f)(h, NVML_TEMPERATURE_GPU, &mut t) == 0).then(|| t as f64)
        }
    }

    pub fn fan_speed(&self, h: isize) -> Option<f64> {
        unsafe {
            let f: Symbol<FnGetFanSpeed> = self.sym(b"nvmlDeviceGetFanSpeed\0")?;
            let mut t = 0u32;
            ((f)(h, &mut t) == 0).then(|| t as f64)
        }
    }

    pub fn power_usage_w(&self, h: isize) -> Option<f64> {
        unsafe {
            let f: Symbol<FnGetPowerUsage> = self.sym(b"nvmlDeviceGetPowerUsage\0")?;
            let mut t = 0u32;
            ((f)(h, &mut t) == 0).then(|| t as f64 / 1000.0)
        }
    }

    pub fn power_limit_w(&self, h: isize) -> Option<f64> {
        unsafe {
            let f: Symbol<FnGetPowerLimit> = self.sym(b"nvmlDeviceGetEnforcedPowerLimit\0")?;
            let mut t = 0u32;
            ((f)(h, &mut t) == 0).then(|| t as f64 / 1000.0)
        }
    }

    pub fn utilization(&self, h: isize) -> Option<(f64, f64)> {
        unsafe {
            let f: Symbol<FnGetUtil> = self.sym(b"nvmlDeviceGetUtilizationRates\0")?;
            let mut u = NvmlUtilization { gpu: 0, memory: 0 };
            ((f)(h, &mut u) == 0).then(|| (u.gpu as f64, u.memory as f64))
        }
    }

    pub fn memory(&self, h: isize) -> Option<(u64, u64)> {
        // (used MB, total MB)
        unsafe {
            let f: Symbol<FnGetMem> = self.sym(b"nvmlDeviceGetMemoryInfo\0")?;
            let mut m = NvmlMemory {
                total: 0,
                free: 0,
                used: 0,
            };
            ((f)(h, &mut m) == 0).then(|| (m.used / 1024 / 1024, m.total / 1024 / 1024))
        }
    }

    pub fn clock_mhz(&self, h: isize, kind: u32) -> Option<u32> {
        unsafe {
            let f: Symbol<FnGetClock> = self.sym(b"nvmlDeviceGetClockInfo\0")?;
            let mut c = 0u32;
            (f(h, kind, &mut c) == 0).then_some(c)
        }
    }

    pub fn max_clock_mhz(&self, h: isize, kind: u32) -> Option<u32> {
        unsafe {
            let f: Symbol<FnGetMaxClock> = self.sym(b"nvmlDeviceGetMaxClockInfo\0")?;
            let mut c = 0u32;
            (f(h, kind, &mut c) == 0).then_some(c)
        }
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        if let Some(f) = self.sym::<FnShutdown>(b"nvmlShutdown\0") {
            unsafe {
                let _ = f();
            }
        }
    }
}
