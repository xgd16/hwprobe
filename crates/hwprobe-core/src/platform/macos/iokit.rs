//! IOKit Registry 与 CoreFoundation 的查询封装。
//!
//! 设计:RAII 包住 IOObject / CF 容器(避免手动 release),取值函数只认类型正确的值 ——
//! 类型不符返回 `None`,让调用方"没有就是没有",不猜不转。
//!
//! 覆盖的接口(`io-kit-sys` / `core-foundation-sys`):
//! - 服务枚举:`IOServiceGetMatchingServices` / `IOIteratorNext`;
//! - 属性读取:`IORegistryEntryCreateCFProperties` / `IORegistryEntryCreateCFProperty`;
//! - CF 取值:`CFDictionaryGetValue` + `CFNumberGetValue` / `CFStringGetCString` /
//!   `CFDataGetBytePtr` / `CFBooleanGetValue`。

use core_foundation_sys::array::{
    CFArrayGetCount, CFArrayGetTypeID, CFArrayGetValueAtIndex, CFArrayRef,
};
use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef};
use core_foundation_sys::data::{CFDataGetBytePtr, CFDataGetLength, CFDataGetTypeID, CFDataRef};
use core_foundation_sys::dictionary::{
    CFDictionaryGetTypeID, CFDictionaryGetValue, CFDictionaryRef, CFMutableDictionaryRef,
};
use core_foundation_sys::number::{
    kCFNumberFloat64Type, kCFNumberSInt64Type, CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef,
    CFNumberGetTypeID, CFNumberGetValue, CFNumberRef,
};
use core_foundation_sys::string::{
    kCFStringEncodingUTF8, CFStringCreateWithCString, CFStringGetCString, CFStringGetTypeID,
    CFStringRef,
};
use io_kit_sys::types::{io_object_t, io_registry_entry_t, io_service_t};
use io_kit_sys::{
    kIOMasterPortDefault, IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IORegistryEntryCreateCFProperty, IORegistryEntryGetName, IORegistryEntryGetRegistryEntryID,
    IOServiceGetMatchingServices, IOServiceMatching,
};
use std::ffi::{CStr, CString};

/// 由 Rust 字符串造一个临时 CFString(Create 语义,用完必须 CFRelease)。
fn cf_string_new(s: &str) -> Option<CFStringRef> {
    let cs = CString::new(s).ok()?;
    // SAFETY: cs 是合法 NUL 结尾字符串;UTF-8 编码;失败返回 null。
    let p =
        unsafe { CFStringCreateWithCString(std::ptr::null(), cs.as_ptr(), kCFStringEncodingUTF8) };
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

// ---- RAII 包装 -------------------------------------------------------------

/// IOObject 句柄,离开作用域自动 `IOObjectRelease`。
pub struct IoObject(io_object_t);

impl IoObject {
    pub fn raw(&self) -> io_service_t {
        self.0
    }
}

impl Drop for IoObject {
    fn drop(&mut self) {
        // SAFETY: self.0 是 IOIteratorNext 返回的有效对象,且只 release 一次。
        unsafe { IOObjectRelease(self.0) };
    }
}

/// CF 容器(通常是从 Copy/Create 语义函数拿来的所有权引用),Drop 时 `CFRelease`。
pub struct CfDict(CFDictionaryRef);

impl CfDict {
    pub fn raw(&self) -> CFDictionaryRef {
        self.0
    }
}

impl Drop for CfDict {
    fn drop(&mut self) {
        // SAFETY: self.0 来自 *Create* 语义函数,所有权归本包装。
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

// ---- 服务枚举 -------------------------------------------------------------

/// 枚举某个 IOService 类(如 `"IOMedia"`、`"IOBlockStorageDevice"`)的全部实例。
pub fn matching_services(class: &str) -> Vec<IoObject> {
    let mut out = Vec::new();
    let Some(cname) = CString::new(class).ok() else {
        return out;
    };
    // SAFETY: IOServiceMatching 返回 +1 的可变字典,交给 IOServiceGetMatchingServices
    // 消费;iterator 有效则逐个 IOIteratorNext 取对象,由 IoObject 负责释放。
    unsafe {
        let matching = IOServiceMatching(cname.as_ptr());
        if matching.is_null() {
            return out;
        }
        let mut iter: io_object_t = 0;
        if IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) != 0 {
            return out;
        }
        loop {
            let obj = IOIteratorNext(iter);
            if obj == 0 {
                break;
            }
            out.push(IoObject(obj));
        }
        IOObjectRelease(iter);
    }
    out
}

/// 取某个 IOService 类的第一个实例。
pub fn matching_service(class: &str) -> Option<IoObject> {
    matching_services(class).into_iter().next()
}

/// 对象在 Registry 里的类名(如 `"AppleSmartBattery"`)。
pub fn object_class(obj: &IoObject) -> Option<String> {
    let mut buf = [0i8; 128];
    // SAFETY: buf 有 128 字节,IORegistryEntryGetName 最多写 128 字节并 NUL 结尾。
    let rc = unsafe { IORegistryEntryGetName(obj.raw() as io_registry_entry_t, buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: buf 是刚才由内核写入的 NUL 结尾字符串。
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Registry 对象的稳定 ID(IORegistryEntryGetRegistryEntryID),用作设备标识。
pub fn entry_id(obj: &IoObject) -> Option<u64> {
    let mut id: u64 = 0;
    // SAFETY: id 是合法出参;成功时内核写入 64 位 ID。
    let rc =
        unsafe { IORegistryEntryGetRegistryEntryID(obj.raw() as io_registry_entry_t, &mut id) };
    (rc == 0 && id != 0).then_some(id)
}

// ---- 属性读取 -------------------------------------------------------------

/// 取 Registry 对象的全部属性字典(IORegistryEntryCreateCFProperties)。
pub fn registry_props(obj: &IoObject) -> Option<CfDict> {
    let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
    // SAFETY: props 出参按 Create 语义返回所有权引用;失败时保持 null。
    let rc = unsafe {
        IORegistryEntryCreateCFProperties(
            obj.raw() as io_registry_entry_t,
            &mut props,
            std::ptr::null(),
            0,
        )
    };
    if rc != 0 || props.is_null() {
        return None;
    }
    Some(CfDict(props as CFDictionaryRef))
}

/// 取 Registry 对象的单个属性(IORegistryEntryCreateCFProperty),用完自动释放。
pub fn entry_prop<T>(
    obj: &IoObject,
    key: &str,
    f: impl FnOnce(CFTypeRef) -> Option<T>,
) -> Option<T> {
    let ckey = cf_string_new(key)?;
    // SAFETY: ckey 是合法 CFString;Create 返回 +1 引用,本函数负责释放。
    unsafe {
        let v = IORegistryEntryCreateCFProperty(
            obj.raw() as io_registry_entry_t,
            ckey,
            std::ptr::null(),
            0,
        );
        CFRelease(ckey as CFTypeRef);
        if v.is_null() {
            return None;
        }
        let out = f(v);
        CFRelease(v);
        out
    }
}

// ---- CF 值转换(类型不符一律 None)----------------------------------------

/// CFNumber → i64(整数或浮点数值都接受,浮点按四舍五入转)。
pub fn cf_i64(v: CFTypeRef) -> Option<i64> {
    if v.is_null() {
        return None;
    }
    // SAFETY: v 是非空 CF 对象;CFGetTypeID 安全,类型不符时不调用 GetValue。
    unsafe {
        let t = CFGetTypeID(v);
        if t == CFNumberGetTypeID() {
            let n = v as CFNumberRef;
            let mut out: i64 = 0;
            if CFNumberGetValue(n, kCFNumberSInt64Type, &mut out as *mut i64 as *mut _) {
                return Some(out);
            }
            let mut f: f64 = 0.0;
            if CFNumberGetValue(n, kCFNumberFloat64Type, &mut f as *mut f64 as *mut _) {
                return Some(f.round() as i64);
            }
            None
        } else if t == CFBooleanGetTypeID() {
            Some(CFBooleanGetValue(v as CFBooleanRef) as i64)
        } else {
            None
        }
    }
}

/// CFNumber → f64。
pub fn cf_f64(v: CFTypeRef) -> Option<f64> {
    if v.is_null() {
        return None;
    }
    // SAFETY: 同 cf_i64,先验类型再取值。
    unsafe {
        if CFGetTypeID(v) != CFNumberGetTypeID() {
            return None;
        }
        let mut f: f64 = 0.0;
        if CFNumberGetValue(
            v as CFNumberRef,
            kCFNumberFloat64Type,
            &mut f as *mut f64 as *mut _,
        ) {
            return Some(f);
        }
        let mut i: i64 = 0;
        if CFNumberGetValue(
            v as CFNumberRef,
            kCFNumberSInt64Type,
            &mut i as *mut i64 as *mut _,
        ) {
            return Some(i as f64);
        }
        None
    }
}

/// CFBoolean / CFNumber → bool。
pub fn cf_bool(v: CFTypeRef) -> Option<bool> {
    if v.is_null() {
        return None;
    }
    // SAFETY: 先验类型再取值。
    unsafe {
        let t = CFGetTypeID(v);
        if t == CFBooleanGetTypeID() {
            return Some(CFBooleanGetValue(v as CFBooleanRef));
        }
        if t == CFNumberGetTypeID() {
            return cf_i64(v).map(|x| x != 0);
        }
        None
    }
}

/// CFString → String。
pub fn cf_string(v: CFTypeRef) -> Option<String> {
    if v.is_null() {
        return None;
    }
    // SAFETY: 先验类型;CFStringGetCString 最多写 buf.len() 字节。
    unsafe {
        if CFGetTypeID(v) != CFStringGetTypeID() {
            return None;
        }
        let mut buf = [0u8; 512];
        let ok = CFStringGetCString(
            v as CFStringRef,
            buf.as_mut_ptr() as *mut _,
            buf.len() as _,
            kCFStringEncodingUTF8,
        );
        if ok == 0 {
            return None;
        }
        CStr::from_ptr(buf.as_ptr() as *const _)
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

/// CFData → 字节切片的拥有型拷贝(避免生命周期纠缠)。
pub fn cf_data(v: CFTypeRef) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    // SAFETY: 先验类型;BytePtr 指向 Length 字节,立即拷出。
    unsafe {
        if CFGetTypeID(v) != CFDataGetTypeID() {
            return None;
        }
        let d = v as CFDataRef;
        let len = CFDataGetLength(d) as usize;
        let p = CFDataGetBytePtr(d);
        if p.is_null() {
            return Some(Vec::new());
        }
        Some(std::slice::from_raw_parts(p, len).to_vec())
    }
}

/// CFArray → 长度(元素访问见 [`array_at`])。
pub fn cf_array_len(v: CFTypeRef) -> Option<usize> {
    if v.is_null() {
        return None;
    }
    // SAFETY: 先验类型再取计数。
    unsafe {
        if CFGetTypeID(v) != CFArrayGetTypeID() {
            return None;
        }
        Some(CFArrayGetCount(v as CFArrayRef) as usize)
    }
}

/// CFArray 第 i 个元素(借用指针,生命周期随数组)。
pub fn cf_array_at(v: CFTypeRef, i: usize) -> Option<CFTypeRef> {
    let n = cf_array_len(v)?;
    if i >= n {
        return None;
    }
    // SAFETY: i < n,取值不越界。
    unsafe { Some(CFArrayGetValueAtIndex(v as CFArrayRef, i as _) as CFTypeRef) }
}

// ---- 字典取值便捷封装 -----------------------------------------------------

/// 从属性字典取整数。
pub fn dict_i64(dict: &CfDict, key: &str) -> Option<i64> {
    dict_value(dict, key).and_then(cf_i64)
}

/// 从属性字典取浮点数(整数存储也接受)。
pub fn dict_f64(dict: &CfDict, key: &str) -> Option<f64> {
    dict_value(dict, key).and_then(|v| cf_f64(v).or_else(|| cf_i64(v).map(|i| i as f64)))
}

/// 从属性字典取字符串。
pub fn dict_string(dict: &CfDict, key: &str) -> Option<String> {
    dict_value(dict, key).and_then(cf_string)
}

/// 从属性字典取布尔。
pub fn dict_bool(dict: &CfDict, key: &str) -> Option<bool> {
    dict_value(dict, key).and_then(cf_bool)
}

/// 从属性字典取原始 CF 值(借用,由字典持有)。
pub fn dict_value(dict: &CfDict, key: &str) -> Option<CFTypeRef> {
    dict_lookup(dict.raw(), key)
}

/// 从裸字典引用取值(借用)。调用方保证 `dict` 有效且是 CFDictionary。
fn dict_lookup(dict: CFDictionaryRef, key: &str) -> Option<CFTypeRef> {
    let ckey = cf_string_new(key)?;
    // SAFETY: dict 有效;CFDictionaryGetValue 返回借用指针,不释放。
    let v = unsafe {
        if CFGetTypeID(dict as CFTypeRef) != CFDictionaryGetTypeID() {
            CFRelease(ckey as CFTypeRef);
            return None;
        }
        let v = CFDictionaryGetValue(dict, ckey as *const _);
        CFRelease(ckey as CFTypeRef);
        v as CFTypeRef
    };
    if v.is_null() {
        None
    } else {
        Some(v)
    }
}

/// 取嵌套字典里的值(如 GPU 的 "PerformanceStatistics" 子字典)。借用,不释放。
pub fn nested_value(dict: &CfDict, sub: &str, key: &str) -> Option<CFTypeRef> {
    let sub_ref = dict_value(dict, sub)?;
    // SAFETY: sub_ref 是借用的 CF 对象;确认是字典后按字典取值。
    unsafe {
        if CFGetTypeID(sub_ref) != CFDictionaryGetTypeID() {
            return None;
        }
    }
    dict_lookup(sub_ref as CFDictionaryRef, key)
}

/// 嵌套字典取整数。
pub fn nested_i64(dict: &CfDict, sub: &str, key: &str) -> Option<i64> {
    nested_value(dict, sub, key).and_then(cf_i64)
}

/// 嵌套字典取浮点。
pub fn nested_f64(dict: &CfDict, sub: &str, key: &str) -> Option<f64> {
    nested_value(dict, sub, key).and_then(|v| cf_f64(v).or_else(|| cf_i64(v).map(|i| i as f64)))
}
