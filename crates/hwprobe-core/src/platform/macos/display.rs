//! Display(显示器/面板)provider(macOS):CoreGraphics 查询活动显示的分辨率、
//! 刷新率与识别信息,CoreVideo 取标称输出刷新率,IOKit 取 EDID 产品名/连接类型。
//!
//! 与 Windows 版(`platform/windows/display.rs`)对齐的部分:
//! - 传感器 code 与声明顺序完全一致:静态 `connector` / `monitor_name` / `touch`,
//!   动态 `resolution` / `refresh_hz`;
//! - macOS 没有 Raw Input 触屏枚举,按 Windows 无触屏机器的同样行为处理 ——
//!   `touch` = "No",**不**出现 `touch_devices`;
//! - refresh 重新枚举,拔出/消失的显示器标 `NotSupported`,不输出旧值。
//!
//! 平台差异(语义对齐、来源不同):
//! - 显示器名取 EDID 产品名(IOKit `EDID` 属性,按 EDID 厂商/型号/序列号与
//!   `CGDisplayVendorNumber/ModelNumber/SerialNumber` 匹配)。取不到时内屏按
//!   `CGDisplayIsBuiltin` 命名为 "Built-in Display",外屏 "Display {i}",
//!   `monitor_name` 外屏退 "Unknown" —— 不猜型号名。
//! - 刷新率两个来源实测对比后选用(见 [`DisplayInfo::refresh_hz`]):
//!   `CGDisplayModeGetRefreshRate` 与
//!   `CVDisplayLinkGetNominalOutputVideoRefreshPeriod`。ProMotion(可变刷新率)
//!   内屏上 CG 返回 0;两者都拿不到就标 `NotSupported`,不猜。
//! - 分辨率优先像素尺寸(`CGDisplayModeGetPixelWidth/Height`,HiDPI 下是面板
//!   真实像素,与 Windows `activeSize` 同口径),缺失回退逻辑点数。

#![cfg(target_os = "macos")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use std::ffi::c_void;

use super::iokit;

// ---- CoreGraphics / CoreVideo FFI ------------------------------------------
// 签名以 macOS SDK 头文件为准(CGDirectDisplay.h、CGDisplayConfiguration.h、
// CVDisplayLink.h、CVBase.h);framework 直接链接,不引第三方 crate。

type CGDirectDisplayID = u32;
type CGDisplayModeRef = *const c_void;
type CVDisplayLinkRef = *mut c_void;

/// `CVBase.h` 的 `CVTime`。`flags` 含 [`KCV_TIME_IS_INDEFINITE`] 时值未定义。
#[repr(C)]
#[derive(Clone, Copy)]
struct CVTime {
    time_value: i64,
    time_scale: i32,
    flags: i32,
}

/// `CVBase.h`:`kCVTimeIsIndefinite = 1 << 0`。
const KCV_TIME_IS_INDEFINITE: i32 = 1 << 0;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    fn CGGetOnlineDisplayList(
        max_displays: u32,
        online_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    // 注意:有两个参数(display, options),options 保留字段传 NULL ——
    // 少写一个参数会把垃圾值当 CFDictionaryRef 传进去,CF 内部解引用野指针。
    fn CGDisplayCopyAllDisplayModes(
        display: CGDirectDisplayID,
        options: *const c_void,
    ) -> *const c_void;
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
    fn CGDisplayModeGetWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetHeight(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: CGDisplayModeRef) -> f64;
    fn CGDisplayIsBuiltin(display: CGDirectDisplayID) -> u32;
    fn CGDisplayVendorNumber(display: CGDirectDisplayID) -> u32;
    fn CGDisplayModelNumber(display: CGDirectDisplayID) -> u32;
    fn CGDisplaySerialNumber(display: CGDirectDisplayID) -> u32;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVDisplayLinkCreateWithCGDisplay(
        display: CGDirectDisplayID,
        display_link_out: *mut CVDisplayLinkRef,
    ) -> i32;
    fn CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link: CVDisplayLinkRef) -> CVTime;
    fn CVDisplayLinkRelease(link: CVDisplayLinkRef);
}

// ---- 原始测量(probe / refresh 与探针共用)---------------------------------

/// 单个活动显示的原始测量值。探针 `dbg_mac_display` 直接打印它,
/// 便于在真机上核对各数据源到底给了什么值。
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: CGDirectDisplayID,
    pub is_main: bool,
    pub is_builtin: bool,
    pub vendor: u32,
    pub model: u32,
    pub serial: u32,
    /// 逻辑点数("UI Looks like")。
    pub width: usize,
    pub height: usize,
    /// 像素尺寸(HiDPI 下是面板真实像素)。
    pub pixel_width: usize,
    pub pixel_height: usize,
    /// `CGDisplayModeGetRefreshRate`:模式自带刷新率,ProMotion 内屏为 0。
    pub cg_refresh_hz: Option<f64>,
    /// `CVDisplayLinkGetNominalOutputVideoRefreshPeriod`:标称输出刷新率。
    pub cv_nominal_hz: Option<f64>,
    /// EDID 产品名(IOKit),内屏通常无 EDID 节点。
    pub edid_name: Option<String>,
    /// IOKit 节点的传输类型描述(如 "DisplayPort" / "HDMI"),用于 `connector`。
    pub transport: Option<String>,
}

impl DisplayInfo {
    /// 稳定设备标识:EDID 厂商/型号/序列号三元组(跨启动、跨插拔稳定,
    /// 类比 Windows 版的 `display:{adapter_id}-{target_id}`)。
    pub fn device_id(&self) -> String {
        format!("display:{}-{}-{}", self.vendor, self.model, self.serial)
    }

    /// 设备名:EDID 产品名 → 内屏 "Built-in Display" → "Display {i}"。
    /// 外屏无 EDID 名时同 Windows 空友好名行为(`Display {i}`)。
    pub fn display_name(&self, index: usize) -> String {
        match &self.edid_name {
            Some(n) => n.clone(),
            None if self.is_builtin => "Built-in Display".to_string(),
            None => format!("Display {index}"),
        }
    }

    /// `monitor_name` 取值:EDID 产品名 → 内屏 "Built-in Display" → "Unknown"
    /// (外屏无名同 Windows 的 "Unknown")。
    pub fn monitor_name(&self) -> String {
        match &self.edid_name {
            Some(n) => n.clone(),
            None if self.is_builtin => "Built-in Display".to_string(),
            None => "Unknown".to_string(),
        }
    }

    /// `connector` 取值:内屏标 embedded(拿不到 eDP/DSI 电气标准,不细分
    /// Windows 的 "LVDS (embedded)" / "DisplayPort (embedded)"),外屏按 IOKit
    /// 传输类型映射到 Windows `tech_name` 同款词汇,拿不到就 "Unknown"。
    pub fn connector(&self) -> &'static str {
        if self.is_builtin {
            return "Embedded (builtin)";
        }
        let t = self.transport.as_deref().unwrap_or("");
        if t.contains("DisplayPort") || t == "DP" {
            "DisplayPort (external)"
        } else if t.contains("HDMI") {
            "HDMI"
        } else {
            "Unknown"
        }
    }

    /// 分辨率字符串:优先像素尺寸,缺失回退逻辑点数。都没有返回 None。
    pub fn resolution(&self) -> Option<String> {
        let (w, h) = if self.pixel_width > 0 && self.pixel_height > 0 {
            (self.pixel_width, self.pixel_height)
        } else if self.width > 0 && self.height > 0 {
            (self.width, self.height)
        } else {
            return None;
        };
        Some(format!("{w}x{h}"))
    }

    /// 刷新率来源选择(实测对比后定,结论见模块文档与探针输出):
    /// 1. `CGDisplayModeGetRefreshRate` —— 模式自带刷新率,固定刷新率显示器准确;
    /// 2. `CVDisplayLinkGetNominalOutputVideoRefreshPeriod` —— 标称输出刷新率,
    ///    CG 返回 0(ProMotion 可变刷新率)时的可靠来源;
    /// 两者都拿不到返回 None,由 refresh 标 `NotSupported`,不猜。
    pub fn refresh_hz(&self) -> Option<f64> {
        self.cg_refresh_hz
            .filter(|hz| *hz > 1.0)
            .or_else(|| self.cv_nominal_hz.filter(|hz| *hz > 1.0))
    }
}

/// 枚举全部活动显示的原始测量值。没有活动显示器返回 Err。
pub fn enumerate() -> Result<Vec<DisplayInfo>, String> {
    let ids = display_ids(false)?;
    if ids.is_empty() {
        return Err("未发现活动的显示器".into());
    }
    // SAFETY: 纯查询,返回主显示器 ID。
    let main = unsafe { CGMainDisplayID() };
    Ok(ids.into_iter().map(|id| describe(id, id == main)).collect())
}

/// 在线显示器的原始测量(**含未激活面板**,如合盖时的 ProMotion 内屏)。
/// provider 只用 [`enumerate`](活动显示器);这个入口给探针核对数据源行为用。
pub fn enumerate_online() -> Result<Vec<DisplayInfo>, String> {
    let ids = display_ids(true)?;
    if ids.is_empty() {
        return Err("未发现在线的显示器".into());
    }
    // SAFETY: 纯查询,返回主显示器 ID。
    let main = unsafe { CGMainDisplayID() };
    Ok(ids.into_iter().map(|id| describe(id, id == main)).collect())
}

/// 该显示器全部受支持模式的刷新率(升序去重;面板能力,不是当前值)。
/// 供探针核对 ProMotion 这类可变刷新率面板的能力范围。
pub fn mode_refresh_rates(id: CGDirectDisplayID) -> Vec<f64> {
    // SAFETY: modes 是 Copy 语义返回的 +1 CFArray,元素为借用引用,
    // 只取值不释放,数组用完 CFRelease。
    unsafe {
        let modes = CGDisplayCopyAllDisplayModes(id, std::ptr::null());
        if modes.is_null() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(n) = iokit::cf_array_len(modes as CFTypeRef) {
            for i in 0..n {
                if let Some(m) = iokit::cf_array_at(modes as CFTypeRef, i) {
                    let hz = CGDisplayModeGetRefreshRate(m as CGDisplayModeRef);
                    if hz.is_finite() && hz > 0.0 {
                        out.push(hz);
                    }
                }
            }
        }
        CFRelease(modes as CFTypeRef);
        out.sort_by(f64::total_cmp);
        out.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        out
    }
}

/// `CGGetActiveDisplayList` / `CGGetOnlineDisplayList`:先取数量再取列表。
fn display_ids(online: bool) -> Result<Vec<CGDirectDisplayID>, String> {
    // SAFETY: maxDisplays=0 + 空指针是官方的"只取数量"用法;第二次调用按
    // 回填数量分配,内核最多写 count 个元素。
    unsafe {
        let get = if online {
            CGGetOnlineDisplayList
        } else {
            CGGetActiveDisplayList
        };
        let mut count = 0u32;
        let err = get(0, std::ptr::null_mut(), &mut count);
        if err != 0 {
            return Err(format!("枚举显示器失败: {err}"));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut ids = vec![0u32; count as usize];
        let err = get(count, ids.as_mut_ptr(), &mut count);
        if err != 0 {
            return Err(format!("枚举显示器失败: {err}"));
        }
        ids.truncate(count as usize);
        Ok(ids)
    }
}

/// 单个显示的原始测量。拿不到的项就是 `None` / 0,不猜。
fn describe(id: CGDirectDisplayID, is_main: bool) -> DisplayInfo {
    // SAFETY: mode 是 Copy 语义返回的 +1 引用,用完 CFRelease;
    // mode 为空时不再调用取值函数。
    let (width, height, pixel_width, pixel_height, cg_refresh_hz) = unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if mode.is_null() {
            (0, 0, 0, 0, None)
        } else {
            let dims = (
                CGDisplayModeGetWidth(mode),
                CGDisplayModeGetHeight(mode),
                CGDisplayModeGetPixelWidth(mode),
                CGDisplayModeGetPixelHeight(mode),
            );
            let hz = CGDisplayModeGetRefreshRate(mode);
            CFRelease(mode as CFTypeRef);
            (
                dims.0,
                dims.1,
                dims.2,
                dims.3,
                hz.is_finite().then_some(hz).filter(|hz| *hz > 0.0),
            )
        }
    };
    // SAFETY: 三个识别函数都是纯查询。
    let (is_builtin, vendor, model, serial) = unsafe {
        (
            CGDisplayIsBuiltin(id) != 0,
            CGDisplayVendorNumber(id),
            CGDisplayModelNumber(id),
            CGDisplaySerialNumber(id),
        )
    };

    // EDID:按 厂商/型号/序列号 与 CG 识别值匹配(两者同源于 EDID 头部)。
    let mut edid_name = None;
    let mut transport = None;
    for (info, t) in edid_displays() {
        if info.vendor == vendor && info.model == model && (serial == 0 || info.serial == serial) {
            edid_name = info.name;
            transport = t;
            break;
        }
    }

    DisplayInfo {
        id,
        is_main,
        is_builtin,
        vendor,
        model,
        serial,
        width,
        height,
        pixel_width,
        pixel_height,
        cg_refresh_hz,
        cv_nominal_hz: cv_nominal_hz(id),
        edid_name,
        transport,
    }
}

/// CoreVideo 标称输出刷新率(Hz)。indefinite 或非法值返回 None。
fn cv_nominal_hz(id: CGDirectDisplayID) -> Option<f64> {
    // SAFETY: out 出参由内核填写;link 是 Create 语义引用,用后 CVDisplayLinkRelease。
    unsafe {
        let mut link: CVDisplayLinkRef = std::ptr::null_mut();
        if CVDisplayLinkCreateWithCGDisplay(id, &mut link) != 0 || link.is_null() {
            return None;
        }
        let t = CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link);
        CVDisplayLinkRelease(link);
        if t.flags & KCV_TIME_IS_INDEFINITE != 0 || t.time_scale <= 0 || t.time_value <= 0 {
            return None;
        }
        let hz = t.time_scale as f64 / t.time_value as f64;
        hz.is_finite().then_some(hz)
    }
}

// ---- EDID(IOKit)----------------------------------------------------------

/// EDID 基础块解析出的识别信息。
#[derive(Debug, Clone, PartialEq)]
struct EdidInfo {
    vendor: u32,
    model: u32,
    serial: u32,
    name: Option<String>,
}

/// 解析 EDID 基础块(不足 128 字节或头部签名不对返回 None,不猜)。
fn parse_edid(bytes: &[u8]) -> Option<EdidInfo> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if bytes.len() < 128 || bytes[..8] != HEADER {
        return None;
    }
    // EDID 头部:厂商 = 大端 15 位 EISA ID;型号 = 小端产品码;序列号 = 小端 u32。
    let vendor = u16::from_be_bytes([bytes[8], bytes[9]]) as u32;
    let model = u16::from_le_bytes([bytes[10], bytes[11]]) as u32;
    let serial = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    // 4 个 18 字节显示器描述符:tag 0xFC = 产品名(13 字节 ASCII,LF 结尾)。
    let name = (0..4)
        .map(|i| &bytes[54 + 18 * i..54 + 18 * (i + 1)])
        .find(|d| d[0] == 0 && d[1] == 0 && d[3] == 0xFC)
        .and_then(|d| {
            let s: String = d[5..18]
                .iter()
                .take_while(|&&c| c != 0x0A && c != 0)
                .map(|&c| c as char)
                .collect();
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        });
    Some(EdidInfo {
        vendor,
        model,
        serial,
        name,
    })
}

/// 扫描 IOKit 显示节点的 EDID 与传输类型。
/// 现代 Apple Silicon 机器外接显示器挂在 `IOPortTransportStateDisplayPort`,
/// Intel 时代是 `IODisplayConnect` / `AppleDisplay`;内屏通常没有 EDID 节点。
fn edid_displays() -> Vec<(EdidInfo, Option<String>)> {
    let mut out = Vec::new();
    for class in [
        "IODisplayConnect",
        "AppleDisplay",
        "IOPortTransportStateDisplayPort",
    ] {
        for obj in iokit::matching_services(class) {
            let Some(bytes) = iokit::entry_prop(&obj, "EDID", iokit::cf_data) else {
                continue;
            };
            let Some(info) = parse_edid(&bytes) else {
                continue;
            };
            let transport = iokit::entry_prop(&obj, "DFP Type Description", iokit::cf_string)
                .or_else(|| iokit::entry_prop(&obj, "TransportTypeDescription", iokit::cf_string));
            out.push((info, transport));
        }
    }
    out
}

// ---- Provider -------------------------------------------------------------

pub struct DisplayProvider;

impl DisplayProvider {
    pub fn new() -> DisplayProvider {
        DisplayProvider
    }
}

impl Default for DisplayProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for DisplayProvider {
    fn describe(&self) -> &str {
        "mac-display"
    }

    fn category(&self) -> Category {
        Category::Display
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let displays = enumerate()?;
        let devices = displays
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let name = d.display_name(i);
                // 传感器 code 与声明顺序同 Windows 版:connector / monitor_name /
                // resolution / refresh_hz / touch。macOS 无触屏枚举,按 Windows
                // 无触屏机器的同样行为:touch = "No",不出现 touch_devices。
                DeviceBuilder::new(d.device_id(), name.clone())
                    .model(name)
                    .static_str("connector", "Connector", d.connector())
                    .static_str("monitor_name", "Monitor Name", d.monitor_name())
                    .dynamic("resolution", "Resolution", "", ValueType::Str)
                    .dynamic("refresh_hz", "Refresh Rate", "Hz", ValueType::F64)
                    .static_str("touch", "Touchscreen", "No")
                    .build()
            })
            .collect();
        Ok(devices)
    }

    fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_millis(2000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        // 分辨率/刷新率可被用户或显示器热插拔改变:刷新时重新查询
        let displays = match enumerate() {
            Ok(d) => d,
            Err(_) => {
                for dev in devices.iter_mut() {
                    dev.set_reading("resolution", Reading::failed(Status::Error, ValueType::Str));
                    dev.set_reading("refresh_hz", Reading::failed(Status::Error, ValueType::F64));
                }
                return;
            }
        };
        for dev in devices.iter_mut() {
            // 按稳定设备 id 匹配(而非下标):热插拔后下标会错位。
            let found = displays.iter().find(|d| d.device_id() == dev.meta.id);
            match found {
                Some(d) => {
                    match d.resolution() {
                        Some(s) => dev.set_reading("resolution", Reading::now(Value::Str(s))),
                        None => dev.set_reading(
                            "resolution",
                            Reading::failed(Status::NotSupported, ValueType::Str),
                        ),
                    }
                    match d.refresh_hz() {
                        Some(hz) => dev.set_reading(
                            "refresh_hz",
                            Reading::now(Value::F64((hz * 1000.0).round() / 1000.0)),
                        ),
                        None => dev.set_reading(
                            "refresh_hz",
                            Reading::failed(Status::NotSupported, ValueType::F64),
                        ),
                    }
                }
                None => {
                    // 显示器在 probe 后被拔出/禁用:标不支持而不是输出旧值
                    dev.set_reading(
                        "resolution",
                        Reading::failed(Status::NotSupported, ValueType::Str),
                    );
                    dev.set_reading(
                        "refresh_hz",
                        Reading::failed(Status::NotSupported, ValueType::F64),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        pixel: (usize, usize),
        logical: (usize, usize),
        cg: Option<f64>,
        cv: Option<f64>,
    ) -> DisplayInfo {
        DisplayInfo {
            id: 1,
            is_main: true,
            is_builtin: false,
            vendor: 8950,
            model: 28194,
            serial: 3229677090,
            width: logical.0,
            height: logical.1,
            pixel_width: pixel.0,
            pixel_height: pixel.1,
            cg_refresh_hz: cg,
            cv_nominal_hz: cv,
            edid_name: None,
            transport: None,
        }
    }

    /// 刷新率来源选择:CG 优先;CG 为 0(ProMotion)回退 CV 标称;都没有 → None。
    #[test]
    fn refresh_source_selection() {
        assert_eq!(
            info((0, 0), (0, 0), Some(60.0), Some(60.0)).refresh_hz(),
            Some(60.0)
        );
        assert_eq!(
            info((0, 0), (0, 0), Some(0.0), Some(60.0)).refresh_hz(),
            Some(60.0)
        );
        assert_eq!(
            info((0, 0), (0, 0), None, Some(59.94)).refresh_hz(),
            Some(59.94)
        );
        assert_eq!(info((0, 0), (0, 0), Some(0.0), None).refresh_hz(), None);
        assert_eq!(
            info((0, 0), (0, 0), Some(0.5), Some(0.5)).refresh_hz(),
            None
        );
    }

    /// 分辨率:优先像素尺寸(HiDPI 真实像素),缺失回退逻辑点数,都没有 → None。
    #[test]
    fn resolution_prefers_pixel_dims() {
        assert_eq!(
            info((3024, 1964), (1512, 982), None, None).resolution(),
            Some("3024x1964".to_string())
        );
        assert_eq!(
            info((0, 0), (1512, 982), None, None).resolution(),
            Some("1512x982".to_string())
        );
        assert_eq!(info((0, 0), (0, 0), None, None).resolution(), None);
    }

    /// EDID 解析:本机外接 MateView 实测字节(厂商 0x22F6 / 型号 0x6E22 /
    /// 序列号 0xC080F622 / 产品名 "MateView",0xFC 描述符的 LF 结尾要截断)。
    #[test]
    fn parse_edid_real_mateview() {
        let hex = "00ffffffffffff0022f6226e22f680c010200104b53c28783a0c95ae5243ae26\
                   0f505421080081008180010101010101010101010101c7f600a0f00049a03020\
                   3a00548d2100001a000000ff0020202020202020202020202020000000fd00\
                   304b2db445000a202020202020000000fc004d617465566965770a20202020\
                   0158020320f0450204105f612309070783010000e305e000e60607016a6a01\
                   e200ca785070a080a0295030203a00548d2100001a565e00a0a0a029503020\
                   3500548d2100001a5898b8a0b0d0397030203a00548d2100001aca9e0050f5\
                   002ea008a03a50548d2100001c94d910a0e060459030203a00548d2100001a\
                   000000000056";
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        let info = parse_edid(&bytes).expect("EDID 应可解析");
        assert_eq!(info.vendor, 0x22F6);
        assert_eq!(info.model, 0x6E22);
        assert_eq!(info.serial, 0xC080_F622);
        assert_eq!(info.name.as_deref(), Some("MateView"));
        // 不足 128 字节基础块的不猜
        assert!(parse_edid(&bytes[..64]).is_none());
    }
}
