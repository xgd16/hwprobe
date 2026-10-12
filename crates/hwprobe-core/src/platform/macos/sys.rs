//! mach / sysctl 基础封装:macOS 后端各 provider 的底层取数入口。
//!
//! - [`sysctl_string`] / [`sysctl_u64`] / [`sysctl_i64`]:`sysctlbyname` 的安全包装;
//! - [`vm_statistics`]:`host_statistics64(HOST_VM_INFO64)` 物理内存统计;
//! - [`cpu_ticks`]:`host_processor_info(PROCESSOR_CPU_LOAD_INFO)` 每核 tick 快照
//!   (自 CPU 启动以来的累计计数,由调用方做时间差分得出占用率)。
//!
//! 常量值以 macOS SDK 头文件为准(`mach/host_info.h`、`mach/processor_info.h`、
//! `mach/machine.h`):`HOST_CPU_LOAD_INFO = 3`、`HOST_VM_INFO64 = 4`、
//! `PROCESSOR_CPU_LOAD_INFO = 2`、`CPU_STATE_MAX = 4`。

use mach2::host_info::HOST_VM_INFO64_COUNT;
use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_init::mach_host_self;
use mach2::message::mach_msg_type_number_t;
use mach2::traps::mach_task_self;
use mach2::vm::mach_vm_deallocate;
use mach2::vm_statistics::vm_statistics64;
use mach2::vm_types::{integer_t, mach_vm_address_t, mach_vm_size_t, natural_t};
use std::ffi::{CStr, CString};

// ---- mach externs(mach2 0.7 未覆盖的 MIG 接口)-----------------------------

/// `mach/processor_info.h`:每核 tick 计数(user / system / idle / nice)。
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessorCpuLoadInfo {
    cpu_ticks: [natural_t; 4],
}

pub const CPU_STATE_USER: usize = 0;
pub const CPU_STATE_SYSTEM: usize = 1;
pub const CPU_STATE_IDLE: usize = 2;
pub const CPU_STATE_NICE: usize = 3;

const HOST_VM_INFO64: i32 = 4;
const PROCESSOR_CPU_LOAD_INFO: i32 = 2;

unsafe extern "C" {
    fn sysctlbyname(
        name: *const libc::c_char,
        oldp: *mut libc::c_void,
        oldlenp: *mut libc::size_t,
        newp: *mut libc::c_void,
        newlen: libc::size_t,
    ) -> libc::c_int;

    fn host_statistics64(
        host: mach2::port::mach_port_t,
        flavor: i32,
        info: *mut vm_statistics64,
        count: *mut mach_msg_type_number_t,
    ) -> i32;

    fn host_processor_info(
        host: mach2::port::mach_port_t,
        flavor: i32,
        out_count: *mut natural_t,
        out_info: *mut *mut integer_t,
        out_info_count: *mut mach_msg_type_number_t,
    ) -> i32;
}

// ---- sysctl ---------------------------------------------------------------

/// 读 sysctl 字符串值(如 `machdep.cpu.brand_string`)。
pub fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut buf = [0u8; 512];
    let mut len = buf.len() as libc::size_t;
    // SAFETY: cname 是合法 NUL 结尾字符串;oldp 指向的 buf 有 len 字节,
    // 内核最多写 len 字节并回填实际长度。返回 0 才读取。
    let rc = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    // 内核回填的长度含 NUL;按 CStr 解析,容忍尾部填充。
    let s = unsafe { CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
    s.to_str().ok().map(str::to_owned)
}

/// 读 sysctl 整数值(自动处理 32/64 位整数;其他类型返回 None)。
pub fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: oldp 为 NULL 只查询长度,不写内存。
    let rc = unsafe {
        sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    match len {
        4 => {
            let mut v: u32 = 0;
            let mut l = 4 as libc::size_t;
            // SAFETY: v 是合法 u32,l 与之匹配。
            let rc = unsafe {
                sysctlbyname(
                    cname.as_ptr(),
                    &mut v as *mut u32 as *mut libc::c_void,
                    &mut l,
                    std::ptr::null_mut(),
                    0,
                )
            };
            (rc == 0).then_some(v as u64)
        }
        8 => {
            let mut v: u64 = 0;
            let mut l = 8 as libc::size_t;
            // SAFETY: v 是合法 u64,l 与之匹配。
            let rc = unsafe {
                sysctlbyname(
                    cname.as_ptr(),
                    &mut v as *mut u64 as *mut libc::c_void,
                    &mut l,
                    std::ptr::null_mut(),
                    0,
                )
            };
            (rc == 0).then_some(v)
        }
        _ => None,
    }
}

/// 读 sysctl 有符号整数值(如 `kern.memorystatus_vm_pressure_level`)。
pub fn sysctl_i64(name: &str) -> Option<i64> {
    sysctl_u64(name).map(|v| v as i64)
}

/// 物理内存页大小(`hw.pagesize`)。
pub fn page_size() -> u64 {
    sysctl_u64("hw.pagesize").unwrap_or(4096)
}

// ---- mach -----------------------------------------------------------------

/// `host_statistics64(HOST_VM_INFO64)`:物理内存统计(单位:页)。
pub fn vm_statistics() -> Option<vm_statistics64> {
    // SAFETY: info 指向可写的 vm_statistics64,count 用 HOST_VM_INFO64_COUNT
    // (mach2 提供,按结构体大小推导)声明容量,与内核约定一致。
    unsafe {
        let mut info: vm_statistics64 = std::mem::zeroed();
        let mut count: mach_msg_type_number_t = HOST_VM_INFO64_COUNT;
        let kr = host_statistics64(mach_host_self(), HOST_VM_INFO64, &mut info, &mut count);
        (kr == KERN_SUCCESS).then_some(info)
    }
}

/// `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`:每核累计 tick 计数
/// (`[user, system, idle, nice]`,单位:tick,自启动累计)。
///
/// 调用方在两个时点各取一次快照做差分,即可得出这段时间的占用率。
pub fn cpu_ticks() -> Option<Vec<[u32; 4]>> {
    // SAFETY: out_info 返回内核分配的数组,长度由 out_info_count 给出;
    // 用后必须 mach_vm_deallocate 归还,下面照做。
    unsafe {
        let mut ncpu: natural_t = 0;
        let mut info: *mut integer_t = std::ptr::null_mut();
        let mut info_count: mach_msg_type_number_t = 0;
        let kr = host_processor_info(
            mach_host_self(),
            PROCESSOR_CPU_LOAD_INFO,
            &mut ncpu,
            &mut info,
            &mut info_count,
        );
        if kr != KERN_SUCCESS || info.is_null() || ncpu == 0 {
            return None;
        }
        let n = ncpu as usize;
        let mut out = Vec::with_capacity(n);
        let typed = info as *const ProcessorCpuLoadInfo;
        for i in 0..n {
            let t = *typed.add(i);
            out.push([
                t.cpu_ticks[CPU_STATE_USER],
                t.cpu_ticks[CPU_STATE_SYSTEM],
                t.cpu_ticks[CPU_STATE_IDLE],
                t.cpu_ticks[CPU_STATE_NICE],
            ]);
        }
        // 归还内核分配的数组(整个 info 数组的字节数 = info_count 个 integer_t)。
        let bytes =
            info_count as mach_vm_size_t * std::mem::size_of::<integer_t>() as mach_vm_size_t;
        mach_vm_deallocate(mach_task_self(), info as mach_vm_address_t, bytes);
        Some(out)
    }
}
