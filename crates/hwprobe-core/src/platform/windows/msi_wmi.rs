//! MSI WMI 平台接口(root\WMI 类 `MSI_ACPI`,GUID
//! `ABBC0F6E-8EA1-11D1-00A0-C90629100000`,源自 Microsoft WMI-ACPI 示例模板)。
//!
//! MSI 自 Tiger Lake 一代起的固件(含 MSI Claw 掌机)经该类暴露 EC 级方法:
//! `GET_FAN`(WmiMethodId 0x11,风扇转速)、`GET_EC`(0x03,EC 固件版本)等。
//! 调用走标准 WMI ExecMethod,**用户态即可,无需内核驱动**(HWiNFO 对
//! MSI Claw 8 的风扇监控即此途径);Linux 侧对应内核 msi-wmi-platform 驱动
//! (drivers/platform/x86/msi-wmi-platform.c)。
//!
//! 协议要点(与内核驱动一致):
//! - 方法输入/输出都是 32 字节缓冲(ACPI CreateByteField 限制);
//! - 输出 [0] 为状态字节,非 0 才有效;
//! - GET_FAN 输出 [1..8] 为 4 通道风扇的大端 u16,RPM = 480000 / 值;
//! - GET_EC 输出 [1] 为 EC 标志位,[2..30] 为 EC 固件版本字符串。
//!
//! 类名在不同固件上可能不同,故先按 `MSI_ACPI` 查找,失败再扫 meta_class
//! 按 `guid` 限定符匹配;方法按 `WmiMethodId` 限定符定位,不依赖方法名。

#![cfg(target_os = "windows")]

use crate::logbuf;
use std::collections::HashMap;
use std::sync::OnceLock;
use windows::core::BSTR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    EOLE_AUTHENTICATION_CAPABILITIES, RPC_C_AUTHN_LEVEL, RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Ole::{
    SafeArrayCreateVector, SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetUBound,
    SafeArrayPutElement,
};
use windows::Win32::System::Variant::{
    VariantClear, VARENUM, VARIANT, VT_ARRAY, VT_BSTR, VT_I4, VT_UI1,
};
use windows::Win32::System::Wmi::{
    IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_NONSYSTEM_ONLY,
    WBEM_GENERIC_FLAG_TYPE, WbemLocator,
};

/// MSI 平台 WMI GUID。
const MSI_PLATFORM_GUID: &str = "ABBC0F6E-8EA1-11D1-00A0-C90629100000";
/// 常见类名(Microsoft WMI-ACPI 模板在 MSI 固件中的落地名)。
const KNOWN_CLASS: &str = "MSI_ACPI";
/// WMI 方法 ID(msi-wmi-platform.c 的 enum msi_wmi_platform_method)。
const METHOD_GET_EC: u32 = 0x03;
const METHOD_GET_FAN: u32 = 0x11;
const METHOD_GET_WMI: u32 = 0x1D;
const BUF_LEN: usize = 32;

// ---------------- 对外接口 ----------------

/// probe 阶段拿到的静态信息。
#[derive(Debug, Clone)]
pub struct MsiWmiInfo {
    /// WMI 接口版本(GET_WMI,形如 "2.0")。
    pub wmi_version: String,
    /// EC 固件版本(GET_EC,如 "14F1EMS1.103")。
    pub ec_firmware: String,
    /// 风扇通道数(读数合理的通道数)。
    pub fans: usize,
}

struct MsiWmiBackend {
    services: IWbemServices,
    /// ExecMethod 目标对象路径(实例路径,回退类路径)。
    object_path: BSTR,
    /// ExecMethod/GetMethod 用的类名。
    class_name: String,
    /// WmiMethodId → 方法名。
    methods: HashMap<u32, String>,
    /// probe 时确认可读的风扇通道(数量在 probe 固定,键保持稳定)。
    fan_channels: usize,
}

unsafe impl Send for MsiWmiBackend {}
unsafe impl Sync for MsiWmiBackend {}

static BACKEND: OnceLock<Option<MsiWmiBackend>> = OnceLock::new();

/// 探测 MSI WMI 平台接口。任何一步失败返回 None(调用方按不支持处理)。
pub fn probe() -> Option<MsiWmiInfo> {
    let backend = BACKEND.get_or_init(init_backend).as_ref()?;
    Some(MsiWmiInfo {
        wmi_version: backend
            .methods
            .get(&METHOD_GET_WMI)
            .and_then(|n| exec_method_buffer(backend, n))
            .filter(|b| b[0] != 0)
            .map(|b| format!("{}.{}", b[1], b[2]))
            .unwrap_or_default(),
        ec_firmware: backend
            .methods
            .get(&METHOD_GET_EC)
            .map(|n| exec_method_buffer(backend, n).map(parse_ec_firmware).unwrap_or_default())
            .unwrap_or_default(),
        fans: backend.fan_channels,
    })
}

/// 刷新风扇转速。返回每通道 RPM(None = 本轮读不到)。
pub fn refresh_fans() -> Vec<Option<f64>> {
    let Some(backend) = BACKEND.get().and_then(|o| o.as_ref()) else {
        return Vec::new();
    };
    let Some(name) = backend.methods.get(&METHOD_GET_FAN) else {
        return Vec::new();
    };
    // 本函数运行在轮询线程:按 COM 规则线程须先加入套间才可调用 COM 接口
    // (加入 MTA 幂等,已初始化时返回 S_FALSE 无碍)。
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        // STA 线程:跨套间调用 MTA 对象不可靠,本轮放弃
        return vec![None; backend.fan_channels];
    }
    let mut rpm = vec![None; backend.fan_channels];
    let Some(bytes) = exec_method_buffer(backend, name) else {
        return rpm;
    };
    if bytes[0] == 0 {
        return rpm;
    }
    for ch in 0..backend.fan_channels {
        let v = ((bytes[1 + ch * 2] as u16) << 8) | bytes[2 + ch * 2] as u16;
        if v > 0 {
            let r = 480_000.0 / v as f64;
            // 合理性过滤:手持机风扇 0~12000 RPM
            if (0.0..=12000.0).contains(&r) {
                rpm[ch] = Some(r.round());
            }
        } else {
            rpm[ch] = Some(0.0);
        }
    }
    rpm
}

/// ExecMethod 失败只记录一次(刷新周期内反复失败不刷屏)。
fn warn_once(method: &str, e: &windows::core::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        logbuf::info(format!("MSI WMI: ExecMethod({method}) 失败(仅记录一次): {e}"));
    }
}

// ---------------- 初始化 ----------------

fn init_backend() -> Option<MsiWmiBackend> {
    unsafe {
        // MTA:wmi crate 的共享连接也是 MTA;STA 线程上无法安全跨线程使用。
        // S_FALSE(已是 MTA)视为成功。
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_err() {
            logbuf::info(format!("MSI WMI: COM 初始化失败: {hr}"));
            return None;
        }

        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| logbuf::info(format!("MSI WMI: 创建 WbemLocator 失败: {e}")))
            .ok()?;

        let empty = BSTR::new();
        let services: IWbemServices = locator
            .ConnectServer(&BSTR::from("ROOT\\WMI"), &empty, &empty, &empty, 0, &empty, None)
            .map_err(|e| logbuf::info(format!("MSI WMI: 连接 ROOT\\WMI 失败: {e}")))
            .ok()?;
        CoSetProxyBlanket(
            &services,
            10, // RPC_C_AUTHN_WINNT
            0,  // RPC_C_AUTHZ_NONE
            windows::core::PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOLE_AUTHENTICATION_CAPABILITIES(0),
        )
        .map_err(|e| logbuf::info(format!("MSI WMI: SetProxyBlanket 失败: {e}")))
        .ok()?;

        let (class, class_name) = find_class(&services)?;
        let methods = enumerate_methods(&class)?;
        // 风扇与 EC 方法必须都在
        methods.get(&METHOD_GET_FAN)?;
        methods.get(&METHOD_GET_EC)?;

        // 实例路径(无实例时用类路径;ExecMethod 两者都接受)
        let object_path =
            instance_path(&services, &class_name).unwrap_or_else(|| BSTR::from(class_name.clone()));

        // 首次 GET_FAN 采样:确认输出有效并数出通道数。
        // 注意:风扇静止(掌机怠速常见)时转速计数为 0,与"通道不存在"不可
        // 区分 —— 状态字节有效时至少保留 1 通道,否则怠速开机永远没有风扇
        // 传感器。Claw 单风扇机型此策略恒正确;多风扇整机在全部静止时少报
        // 后置风扇,转动后也不会补报(probe 期固定),可接受。
        let fan_name = methods[&METHOD_GET_FAN].clone();
        let bytes = exec_method_buffer_raw(&services, &object_path, &class_name, &fan_name)?;
        let mut fan_channels = 0usize;
        if bytes[0] != 0 {
            for ch in 0..4 {
                let v = ((bytes[1 + ch * 2] as u16) << 8) | bytes[2 + ch * 2] as u16;
                if v == 0 {
                    continue;
                }
                let r = 480_000.0 / v as f64;
                if (0.0..=12000.0).contains(&r) {
                    fan_channels = ch + 1;
                }
            }
            if fan_channels == 0 {
                // 状态有效但全部计数为 0 = 风扇存在且静止
                fan_channels = 1;
            }
        }
        if fan_channels == 0 {
            logbuf::info("MSI WMI: GET_FAN 无有效读数(状态字节为 0),不声明风扇传感器");
            return None;
        }

        logbuf::info(format!(
            "MSI WMI 平台就绪: 类 {class_name}, EC 固件 {}, 风扇通道 {fan_channels}",
            methods
                .get(&METHOD_GET_EC)
                .map(|n| exec_method_buffer_raw(&services, &object_path, &class_name, n)
                    .map(parse_ec_firmware)
                    .unwrap_or_default())
                .unwrap_or_default()
        ));

        Some(MsiWmiBackend {
            services,
            object_path,
            class_name,
            methods,
            fan_channels,
        })
    }
}

/// 查找目标 WMI 类:先试已知类名,失败再扫 meta_class 按 guid 限定符匹配。
/// 返回类对象与类名(供后续 GetObject/ExecMethod 使用)。
fn find_class(services: &IWbemServices) -> Option<(IWbemClassObject, String)> {
    unsafe {
        let mut obj_slot: Option<IWbemClassObject> = None;
        if services
            .GetObject(
                &BSTR::from(KNOWN_CLASS),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut obj_slot),
                None,
            )
            .is_ok()
        {
            let obj = obj_slot?;
            return Some((obj, KNOWN_CLASS.to_string()));
        }

        // 回退:枚举所有类定义,按 guid 限定符匹配(WMI-ACPI 类名因厂而异)
        let guid = BSTR::from(MSI_PLATFORM_GUID);
        let enum_obj = services
            .ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT * FROM meta_class"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
            )
            .ok()?;
        let mut batch: [Option<IWbemClassObject>; 1] = [None];
        for _ in 0..2000 {
            let mut returned = 0u32;
            if enum_obj.Next(1000, &mut batch, &mut returned).is_err() || returned == 0 {
                break;
            }
            let Some(obj) = batch[0].take() else { break };
            let Some(qs) = obj.GetQualifierSet().ok() else { continue };
            let mut v = VARIANT::default();
            if qs.Get(&BSTR::from("guid"), 0, &mut v, &mut 0).is_err() {
                continue;
            }
            let matches = unsafe {
                v.Anonymous.Anonymous.vt == VT_BSTR
                    && *v.Anonymous.Anonymous.Anonymous.bstrVal == guid
            };            let _ = VariantClear(&mut v);
            if matches {
                // 取 __CLASS 作为后续 GetObject 的路径
                let mut cv = VARIANT::default();
                let name = obj
                    .Get(&BSTR::from("__CLASS"), 0, &mut cv, None, None)
                    .ok()
                    .and_then(|_| {
                        let s = unsafe {
                            if cv.Anonymous.Anonymous.vt == VT_BSTR {
                                Some((*cv.Anonymous.Anonymous.Anonymous.bstrVal).to_string())
                            } else {
                                None
                            }
                        };
                        let _ = VariantClear(&mut cv);
                        s
                    })
                    .unwrap_or_else(|| KNOWN_CLASS.to_string());
                return Some((obj, name));
            }
        }
        logbuf::info(format!("MSI WMI: 未找到 GUID {MSI_PLATFORM_GUID} 对应的类"));
        None
    }
}

/// 枚举类方法,返回 WmiMethodId → 方法名(上限 64,MSI_ACPI 约 28 个;
/// WBEM_S_NO_MORE_DATA 在 windows crate 中不表现为 Err,以空名终止)。
fn enumerate_methods(class: &IWbemClassObject) -> Option<HashMap<u32, String>> {
    let mut map = HashMap::new();
    unsafe {
        if class.BeginMethodEnumeration(0).is_err() {
            return None;
        }
        for _ in 0..64 {
            let mut name = BSTR::new();
            let mut in_sig: Option<IWbemClassObject> = None;
            let mut out_sig: Option<IWbemClassObject> = None;
            if class.NextMethod(0, &mut name, &mut in_sig, &mut out_sig).is_err() || name.is_empty()
            {
                break;
            }
            let Ok(qs) = class.GetMethodQualifierSet(&name) else {
                continue;
            };
            let mut v = VARIANT::default();
            if qs.Get(&BSTR::from("WmiMethodId"), 0, &mut v, &mut 0).is_err() {
                continue;
            }
            if unsafe { v.Anonymous.Anonymous.vt } == VT_I4 {
                let id = unsafe { v.Anonymous.Anonymous.Anonymous.lVal } as u32;
                map.insert(id, name.to_string());
            }
            let _ = VariantClear(&mut v);
        }
        let _ = class.EndMethodEnumeration();
    }
    (!map.is_empty()).then_some(map)
}

fn instance_path(services: &IWbemServices, class_name: &str) -> Option<BSTR> {
    unsafe {
        let enum_obj = services
            .CreateInstanceEnum(&BSTR::from(class_name), WBEM_GENERIC_FLAG_TYPE(0), None)
            .ok()?;
        let mut batch: [Option<IWbemClassObject>; 1] = [None];
        let mut returned = 0u32;
        if enum_obj.Next(1000, &mut batch, &mut returned).is_err() {
            return None;
        }
        if returned == 0 {
            return None;
        }
        let obj = batch[0].take()?;
        let mut v = VARIANT::default();
        if obj.Get(&BSTR::from("__PATH"), 0, &mut v, None, None).is_err() {
            return None;
        }
        let path = (unsafe { v.Anonymous.Anonymous.vt } == VT_BSTR)
            .then(|| unsafe { (*v.Anonymous.Anonymous.Anonymous.bstrVal).to_string() })
            .map(BSTR::from);
        let _ = VariantClear(&mut v);
        path
    }
}

// ---------------- 方法调用 ----------------

fn exec_method_buffer(backend: &MsiWmiBackend, method: &str) -> Option<[u8; BUF_LEN]> {
    exec_method_buffer_raw(&backend.services, &backend.object_path, &backend.class_name, method)
}

fn exec_method_buffer_raw(
    services: &IWbemServices,
    object_path: &BSTR,
    class_name: &str,
    method: &str,
) -> Option<[u8; BUF_LEN]> {
    unsafe {
        let mut class_slot: Option<IWbemClassObject> = None;
        services
            .GetObject(
                &BSTR::from(class_name),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut class_slot),
                None,
            )
            .ok()?;
        let class = class_slot?;
        let mut in_sig: Option<IWbemClassObject> = None;
        let mut out_sig: Option<IWbemClassObject> = None;
        class.GetMethod(&BSTR::from(method), 0, &mut in_sig, &mut out_sig).ok()?;

        // 输入参数:ACPI 侧固定要求 32 字节缓冲;WMI 签名里的字节数组属性
        // 统一置为 32 字节 0(内核驱动同款全零输入)。
        let in_params = in_sig.and_then(|sig| {
            let inst = sig.SpawnInstance(0).ok()?;
            prepare_input_buffer(&inst, method);
            Some(inst)
        });

        let mut out_params: Option<IWbemClassObject> = None;
        services
            .ExecMethod(
                object_path,
                &BSTR::from(method),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                in_params.as_ref(),
                Some(&mut out_params),
                None,
            )
            .map_err(|e| warn_once(method, &e))
            .ok()?;

        extract_buffer(&out_params?, method)
    }
}

/// 把输入签名里所有 UI1 数组属性统一置为 32 字节 0(ACPI CreateByteField 要求)。
fn prepare_input_buffer(inst: &IWbemClassObject, method: &str) {
    unsafe {
        if inst.BeginEnumeration(WBEM_FLAG_NONSYSTEM_ONLY.0).is_err() {
            return;
        }
        let mut names = Vec::new();
        loop {
            let mut name = BSTR::new();
            let mut v = VARIANT::default();
            if inst.Next(0, &mut name, &mut v, &mut 0, &mut 0).is_err() || name.is_empty() {
                break;
            }
            let _ = VariantClear(&mut v);
            names.push(name);
        }
        let _ = inst.EndEnumeration();
        for name in names {
            let mut v = VARIANT::default();
            if inst.Get(&name, 0, &mut v, None, None).is_err() {
                continue;
            }
            let vt = unsafe { v.Anonymous.Anonymous.vt };
            let is_arr = (vt.0 & VT_ARRAY.0) != 0 && (vt.0 & VT_UI1.0) != 0;
            let _ = VariantClear(&mut v);
            if !is_arr {
                continue;
            }
            let Some(arr) = zero_array(BUF_LEN as u32) else {
                continue;
            };
            let mut var = VARIANT::default();
            // VARIANT_0.Anonymous 是 ManuallyDrop,写侧显式解包(隐式 DerefMut 被禁止)
            let v00 = &mut *var.Anonymous.Anonymous;
            v00.vt = VARENUM(VT_ARRAY.0 | VT_UI1.0);
            v00.Anonymous.parray = arr;
            let put = inst.Put(&name, 0, &var, 0);
            let _ = SafeArrayDestroy(arr);
            if put.is_err() {
                logbuf::info(format!("MSI WMI: 输入参数({method})设置失败"));
            }
        }
    }
}

/// 从输出对象中提取 UI1 数组为字节缓冲。
fn extract_buffer(out: &IWbemClassObject, method: &str) -> Option<[u8; BUF_LEN]> {
    unsafe {
        if out.BeginEnumeration(WBEM_FLAG_NONSYSTEM_ONLY.0).is_err() {
            return None;
        }
        let mut result = None;
        loop {
            let mut name = BSTR::new();
            let mut v = VARIANT::default();
            if out.Next(0, &mut name, &mut v, &mut 0, &mut 0).is_err() || name.is_empty() {
                break;
            }
            let vt = v.Anonymous.Anonymous.vt;
            if (vt.0 & VT_ARRAY.0) != 0 && (vt.0 & VT_UI1.0) != 0 {
                let psa = v.Anonymous.Anonymous.Anonymous.parray;
                if let Some(bytes) = read_array_bytes(psa) {
                    if bytes[0] == 0 {
                        logbuf::info(format!("MSI WMI: {method} 返回状态字节为 0"));
                    }
                    result = Some(bytes);
                }
            }
            let _ = VariantClear(&mut v);
            if result.is_some() {
                break;
            }
        }
        let _ = out.EndEnumeration();
        result
    }
}

/// VT_UI1 数组按元素读出。SafeArrayGetElement 的 pv 必须是元素本身大小
/// 的缓冲(1 字节);只有 VT_VARIANT 数组才按 VARIANT 整体处理,
/// 传 VARIANT 会把元素写到 vt 低字节而 bVal(偏移 8)恒读出 0。
fn read_array_bytes(psa: *mut windows::Win32::System::Com::SAFEARRAY) -> Option<[u8; BUF_LEN]> {
    unsafe {
        let ub = SafeArrayGetUBound(psa, 1).ok()?;
        if (ub as usize + 1) < 2 {
            return None;
        }
        let mut out = [0u8; BUF_LEN];
        for i in 0..(ub as usize + 1).min(BUF_LEN) {
            let mut b = 0u8;
            SafeArrayGetElement(psa, &(i as i32), &mut b as *mut u8 as *mut std::ffi::c_void)
                .ok()?;
            out[i] = b;
        }
        Some(out)
    }
}

/// len 字节全零输入缓冲。PutElement 同样按元素大小传 1 字节指针;
/// 出错时销毁已创建的 SAFEARRAY,避免泄漏。
fn zero_array(len: u32) -> Option<*mut windows::Win32::System::Com::SAFEARRAY> {
    unsafe {
        let psa = SafeArrayCreateVector(VT_UI1, 0, len);
        if psa.is_null() {
            return None;
        }
        let zero = 0u8;
        for i in 0..len {
            if SafeArrayPutElement(
                psa,
                &(i as i32),
                &zero as *const u8 as *const std::ffi::c_void,
            )
            .is_err()
            {
                SafeArrayDestroy(psa);
                return None;
            }
        }
        Some(psa)
    }
}

/// GET_EC 输出:[0] 状态字节,[1] 标志位,[2..30] 固件版本字符串(内核同款偏移)。
fn parse_ec_firmware(bytes: [u8; BUF_LEN]) -> String {
    if bytes[0] == 0 {
        return String::new();
    }
    let end = bytes[2..2 + 28]
        .iter()
        .position(|&c| c == 0 || !c.is_ascii_graphic())
        .unwrap_or(28);
    String::from_utf8_lossy(&bytes[2..2 + end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 风扇换算:RPM = 480000 / BE16 值(与内核 msi-wmi-platform 一致)。
    #[test]
    fn fan_rpm_formula() {
        let bytes = [1u8, 0, 120, 0, 0, 0, 0, 0, 0];
        let v = ((bytes[1] as u16) << 8) | bytes[2] as u16;
        assert_eq!(480_000.0 / v as f64, 4000.0);
    }

    /// EC 固件字符串解析:截断于 NUL / 非可打印字符,偏移 2。
    #[test]
    fn ec_firmware_parse() {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[1] = 0x80;
        bytes[2..=11].copy_from_slice(b"14F1EMS1.1");
        assert_eq!(parse_ec_firmware(bytes), "14F1EMS1.1");

        bytes[12] = 0;
        assert_eq!(parse_ec_firmware(bytes), "14F1EMS1.1");

        bytes[0] = 0; // 状态字节为 0 → 无效
        assert_eq!(parse_ec_firmware(bytes), "");
    }
}
