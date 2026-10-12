//! CoreWLAN 桥:WiFi 关联细节(SSID/BSSID/RSSI/噪声/协商速率/信道/频段/加密/射频开关)。
//!
//! CoreWLAN 是 Objective-C API,不引入 ObjC 绑定 crate,直接经
//! `objc_msgSend` 裸桥调用(见下),类与 selector 在运行期经
//! `objc_getClass` / `sel_registerName` 解析 —— 所以 WiFi 栈不可用的机器上
//! 只是"查不到接口",不会链接失败或崩溃。
//!
//! 常量与方法签名以本机 SDK 头文件为准(`MacOSX.sdk`):
//! - `CoreWLAN.framework/Headers/CWInterface.h`:`powerOn` / `ssid` / `bssid` /
//!   `rssiValue` / `noiseMeasurement` / `transmitRate` / `wlanChannel` /
//!   `security` / `interfaceMode` / `activePHYMode`;
//! - `CoreWLAN.framework/Headers/CWChannel.h`:`channelNumber` / `channelBand`;
//! - `CoreWLAN.framework/Headers/CoreWLANTypes.h`:`CWSecurity`(0..15 + `NSIntegerMax`)、
//!   `CWChannelBand`(0..3)、`CWInterfaceMode`(0..3)、`CWPHYMode`(0..7)的显式取值
//!   —— 枚举名不按序号猜,只认头文件里写出来的数字。
//!
//! 现实约束(取自 CWInterface.h 的文档注释,不是猜的):
//! - `ssid` / `bssid` **需要定位服务(Location Services)授权**,未授权返回 nil:
//!   此时如实报"无权限",绝不用 BSSID/信道反推 SSID。
//! - `rssiValue` / `noiseMeasurement` 未关联时返回 0,`wlanChannel` 返回 nil
//!   —— 据此判"是否已关联",0 不当成读数上报。
//! - macOS 只暴露**发射**协商速率(`transmitRate`),没有接收协商速率的接口。
//! - 射频开关只有软件电源状态(`powerOn`),没有独立的硬件开关查询。
//!
//! 线程模型:与 Windows 版 `Wlan` 一致,句柄不做并发保证,由 `NetProvider`
//! 用 `Mutex` 串行化(故只声明 `Send`)。

use crate::platform::macos::iokit::cf_string;
use core_foundation_sys::base::CFTypeRef;
use std::ffi::{c_char, c_void, CStr};

// ---- ObjC runtime 裸桥 ----------------------------------------------------
//
// 本文件只用到"无参取值"与"带一个对象参数"两类消息形态,返回值是对象指针 /
// NSInteger / double / BOOL —— 全部走寄存器返回,arm64 与 x86_64 上都不涉及
// stret 变体(返回结构体才有),因此可以按返回类型分型声明同一个符号。
//
// 同一个 `objc_msgSend` 声明成多个签名是这个裸桥的标准做法(它的真签名随方法
// 返回类型而变,只此一途),`clashing_extern_declarations` 在这里是误报。
#[allow(clashing_extern_declarations)]
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *const c_void;
    fn sel_registerName(name: *const c_char) -> *const c_void;

    #[link_name = "objc_msgSend"]
    fn msg_obj(this: *const c_void, sel: *const c_void) -> *const c_void;
    #[link_name = "objc_msgSend"]
    fn msg_isize(this: *const c_void, sel: *const c_void) -> isize;
    #[link_name = "objc_msgSend"]
    fn msg_f64(this: *const c_void, sel: *const c_void) -> f64;
    #[link_name = "objc_msgSend"]
    fn msg_bool(this: *const c_void, sel: *const c_void) -> u8;
    #[link_name = "objc_msgSend"]
    fn msg_void(this: *const c_void, sel: *const c_void);
    #[link_name = "objc_msgSend"]
    fn msg_obj_arg(this: *const c_void, sel: *const c_void, arg: *const c_void) -> *const c_void;
}

// CoreWLAN / Foundation 必须真的被链接进来:这两个框架的符号一个都不直接引用
// (全走 objc_msgSend),所以用空 extern 块把链接意图显式钉住 —— 否则框架不进
// 进程映像,objc_getClass 取不到类,整组 WiFi 键会莫名全是"不支持"。
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreWLAN", kind = "framework")]
unsafe extern "C" {}

// ---- 运行期小工具 ---------------------------------------------------------

fn class(name: &CStr) -> *const c_void {
    // SAFETY: name 是合法 NUL 结尾 C 字符串;类不存在时返回 null。
    unsafe { objc_getClass(name.as_ptr()) }
}

fn sel(name: &CStr) -> *const c_void {
    // SAFETY: 同上;sel_registerName 对同一个名字总是返回同一个 SEL,可安全复用。
    unsafe { sel_registerName(name.as_ptr()) }
}

/// `NSAutoreleasePool`:CoreWLAN 返回的字符串、通道对象全是 autorelease 的,
/// 而轮询线程上没有池 —— 不包一层就逐次泄漏(运行期还会一直记
/// "Object autoreleased without pool" 警告)。每次查询开一个,出作用域 drain。
struct Pool(*const c_void);

impl Pool {
    fn new() -> Pool {
        let cls = class(c"NSAutoreleasePool");
        let p = if cls.is_null() {
            std::ptr::null()
        } else {
            // SAFETY: cls 是 NSAutoreleasePool 类对象;alloc/init 返回 +1 对象,
            // 交给 Drop 里的 drain 释放(池对象本身不可 retain/autorelease)。
            unsafe { msg_obj(msg_obj(cls, sel(c"alloc")), sel(c"init")) }
        };
        Pool(p)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 是 init 出来的池对象,drain 即释放。
            unsafe { msg_void(self.0, sel(c"drain")) };
        }
    }
}

/// NSString* → Rust String。NSString 与 CFString 免桥接,直接复用
/// [`cf_string`](super::iokit::cf_string) 的类型检查与转换。
fn ns_string(obj: *const c_void) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    cf_string(obj as CFTypeRef)
}

/// CoreWLAN 会话(取 `CWWiFiClient` 共享单例)。
///
/// 句柄不做并发保证,调用方须串行化(`NetProvider` 用 `Mutex` 持有)。
/// 内部是 ObjC 对象指针,此处声明可跨线程移动:调用始终在同一把锁下进行。
pub struct Wlan {
    client: *const c_void,
}

unsafe impl Send for Wlan {}

/// 一次查询得到的关联细节。未关联时 `connected == false`,关联类字段为 `None`。
#[derive(Default, Debug, Clone)]
pub struct WifiDetails {
    /// 接口状态名,与 Windows 版 `wifi_state` 的词汇保持一致。
    pub state: &'static str,
    pub connected: bool,
    /// 射频开关(软件电源状态)。macOS 无独立硬件开关查询。
    pub radio_on: Option<bool>,
    pub ssid: Option<String>,
    pub bssid: Option<String>,
    pub rssi_dbm: Option<i32>,
    /// 噪声底限(dBm)。macOS 独有,Windows WLAN API 不给。
    pub noise_dbm: Option<i32>,
    /// 发射协商速率(Mbps)。macOS 没有接收协商速率接口。
    pub tx_mbps: Option<f64>,
    pub channel: Option<u32>,
    /// 频段(`2.4 GHz` / `5 GHz` / `6 GHz`,字符串与 Windows 版一致)。
    pub band: Option<&'static str>,
    /// 制式(`802.11n` 等,字符串与 Windows 版 `wifi_phy` 一致)。
    pub phy: Option<&'static str>,
    /// 安全模式(`WPA2-Personal` 等;macOS 给的是认证+加密的合并枚举)。
    pub security: Option<&'static str>,
}

impl WifiDetails {
    /// 信道 → 主信道中心频率(MHz)。频段或信道未知时返回 `None`(不猜)。
    ///
    /// 换算是 IEEE 802.11 的定义而不是近似:与 Windows 版从 BSS 条目读到的
    /// `ulChCenterFrequency` 同语义(都是主信道中心,不是 80/160 MHz 的合信道中心)。
    pub fn freq_mhz(&self) -> Option<f64> {
        freq_mhz(self.band?, self.channel?)
    }
}

impl Wlan {
    /// 打开 CoreWLAN 会话。WiFi 栈不可用(无 WiFi 硬件/框架)时返回 `None`。
    pub fn open() -> Option<Wlan> {
        let cls = class(c"CWWiFiClient");
        if cls.is_null() {
            return None;
        }
        let client = unsafe { msg_obj(cls, sel(c"sharedWiFiClient")) };
        if client.is_null() {
            return None;
        }
        Some(Wlan { client })
    }

    /// 该 BSD 名(如 `"en0"`)是不是 WLAN 站点接口。
    ///
    /// 有线/虚拟接口上 `interfaceWithName:` 返回 nil —— 与 Windows 版
    /// `WlanEnumInterfaces` 的判定语义一致:Wi-Fi Direct 之类的虚拟 802.11 设备
    /// 查不到任何 WiFi 数据,上层据此决定要不要挂 WiFi 扩展键,免得挂一排错键。
    pub fn has_interface(&self, name: &str) -> bool {
        let _pool = Pool::new();
        self.iface(name).is_some()
    }

    /// 查询当前关联细节。接口在 CoreWLAN 里不存在时返回 `None`。
    pub fn query(&self, name: &str) -> Option<WifiDetails> {
        let _pool = Pool::new();
        let iface = self.iface(name)?;

        let power_on = unsafe { msg_bool(iface, sel(c"powerOn")) } != 0;
        let mode = unsafe { msg_isize(iface, sel(c"interfaceMode")) };
        let channel_obj = unsafe { msg_obj(iface, sel(c"wlanChannel")) };
        let rssi = unsafe { msg_isize(iface, sel(c"rssiValue")) } as i32;
        let noise = unsafe { msg_isize(iface, sel(c"noiseMeasurement")) } as i32;
        let tx = unsafe { msg_f64(iface, sel(c"transmitRate")) };
        let security = unsafe { msg_isize(iface, sel(c"security")) };
        let phy = unsafe { msg_isize(iface, sel(c"activePHYMode")) };

        // SSID/BSSID 要定位服务授权;读不到就是读不到,不从别处反推。
        let ssid = ns_string(unsafe { msg_obj(iface, sel(c"ssid")) });
        let bssid = ns_string(unsafe { msg_obj(iface, sel(c"bssid")) });

        let channel = if channel_obj.is_null() {
            None
        } else {
            Some(unsafe { msg_isize(channel_obj, sel(c"channelNumber")) } as u32)
        };
        let band = if channel_obj.is_null() {
            None
        } else {
            band_name(unsafe { msg_isize(channel_obj, sel(c"channelBand")) })
        };

        // 文档:rssiValue / noiseMeasurement 未关联时返回 0 —— 0 不是读数。
        let connected = mode != 0 || !channel_obj.is_null();

        Some(WifiDetails {
            state: if !power_on {
                "Not Ready"
            } else if connected {
                "Connected"
            } else {
                "Disconnected"
            },
            connected,
            radio_on: Some(power_on),
            ssid,
            bssid,
            rssi_dbm: (connected && rssi != 0).then_some(rssi),
            noise_dbm: (connected && noise != 0).then_some(noise),
            tx_mbps: (connected && tx > 0.0).then_some(tx),
            channel,
            band,
            phy: phy_name(phy),
            security: security_name(security),
        })
    }

    /// 网卡硬件地址(MAC)。CoreWLAN 按 BSD 名直接给**硬件地址**,与
    /// `networksetup -getmacaddress` 一致 —— 而 getifaddrs 在新系统上会被隐私
    /// 屏蔽成占位值(见 `net.rs`),`ifconfig` 报的则是私有随机地址(开了
    /// "私有 Wi-Fi 地址"时),两者不同时是正常现象。这里取硬件地址。
    pub fn hardware_address(&self, name: &str) -> Option<String> {
        let _pool = Pool::new();
        let iface = self.iface(name)?;
        ns_string(unsafe { msg_obj(iface, sel(c"hardwareAddress")) })
    }

    /// 按 BSD 名取 `CWInterface`(autoreleased,须活在 [`Pool`] 作用域里)。
    fn iface(&self, name: &str) -> Option<*const c_void> {
        let cname = std::ffi::CString::new(name).ok()?;
        // NSString 由 Foundation 构造(框架已在文件头的 #[link] 里钉住)。
        let ns_name = unsafe {
            msg_obj_arg(
                class(c"NSString"),
                sel(c"stringWithUTF8String:"),
                cname.as_ptr() as *const c_void,
            )
        };
        if ns_name.is_null() {
            return None;
        }
        let obj = unsafe { msg_obj_arg(self.client, sel(c"interfaceWithName:"), ns_name) };
        if obj.is_null() {
            None
        } else {
            Some(obj)
        }
    }
}

// ---- 枚举解码(取值来自 CoreWLANTypes.h)----------------------------------

/// `CWChannelBand` → 频段名。字符串与 Windows 版 `band_from_khz` 的返回值一致。
fn band_name(v: isize) -> Option<&'static str> {
    match v {
        1 => Some("2.4 GHz"),
        2 => Some("5 GHz"),
        3 => Some("6 GHz"),
        _ => None, // 0 = kCWChannelBandUnknown
    }
}

/// `CWPHYMode` → 制式名。字符串与 Windows 版 `phy_name` 的返回值一致。
fn phy_name(v: isize) -> Option<&'static str> {
    match v {
        1 => Some("802.11a"),
        2 => Some("802.11b"),
        3 => Some("802.11g"),
        4 => Some("802.11n"),
        5 => Some("802.11ac"),
        6 => Some("802.11ax"),
        7 => Some("802.11be"),
        _ => None, // 0 = kCWPHYModeNone
    }
}

/// `CWSecurity` → 安全模式名。
///
/// macOS 给的是"认证 + 加密"的合并枚举,而 Windows 是 `wifi_auth` / `wifi_cipher`
/// 两个键:这里把合并值放进 auth 侧(它确实描述了认证方式),加密算法则如实
/// 标不支持 —— 拆出一个"推断的 cipher"就是编数据了。
fn security_name(v: isize) -> Option<&'static str> {
    match v {
        0 => Some("Open"),
        1 => Some("WEP"),
        2 => Some("WPA-Personal"),
        3 => Some("WPA-Personal Mixed"),
        4 => Some("WPA2-Personal"),
        5 => Some("WPA/WPA2-Personal"),
        6 => Some("Dynamic WEP"),
        7 => Some("WPA-Enterprise"),
        8 => Some("WPA-Enterprise Mixed"),
        9 => Some("WPA2-Enterprise"),
        10 => Some("WPA/WPA2-Enterprise"),
        11 => Some("WPA3-Personal"),
        12 => Some("WPA3-Enterprise"),
        13 => Some("WPA3-Transition"),
        14 => Some("OWE"),
        15 => Some("OWE Transition"),
        _ => None, // kCWSecurityUnknown = NSIntegerMax
    }
}

/// 主信道中心频率(MHz)。见 [`WifiDetails::freq_mhz`] 的语义说明。
fn freq_mhz(band: &str, channel: u32) -> Option<f64> {
    let mhz = match band {
        // 2.4 GHz:信道 1..13 是 2407+5n,信道 14 单独是 2484(802.11b 专有)。
        "2.4 GHz" if (1..=13).contains(&channel) => 2407 + 5 * channel,
        "2.4 GHz" if channel == 14 => 2484,
        // 5 GHz / 6 GHz:5000+5n 与 5950+5n。
        "5 GHz" if (1..=196).contains(&channel) => 5000 + 5 * channel,
        "6 GHz" if (1..=233).contains(&channel) => 5950 + 5 * channel,
        _ => return None,
    };
    Some(mhz as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_and_phy_names() {
        assert_eq!(band_name(1), Some("2.4 GHz"));
        assert_eq!(band_name(2), Some("5 GHz"));
        assert_eq!(band_name(3), Some("6 GHz"));
        assert_eq!(band_name(0), None);
        assert_eq!(band_name(4), None);
        assert_eq!(phy_name(4), Some("802.11n"));
        assert_eq!(phy_name(5), Some("802.11ac"));
        assert_eq!(phy_name(6), Some("802.11ax"));
        assert_eq!(phy_name(7), Some("802.11be"));
        assert_eq!(phy_name(0), None);
    }

    #[test]
    fn security_names_match_header() {
        assert_eq!(security_name(0), Some("Open"));
        assert_eq!(security_name(4), Some("WPA2-Personal"));
        assert_eq!(security_name(11), Some("WPA3-Personal"));
        assert_eq!(security_name(14), Some("OWE"));
        assert_eq!(security_name(15), Some("OWE Transition"));
        // kCWSecurityUnknown = NSIntegerMax
        assert_eq!(security_name(isize::MAX), None);
    }

    #[test]
    fn freq_from_channel() {
        assert_eq!(freq_mhz("2.4 GHz", 1), Some(2412.0));
        assert_eq!(freq_mhz("2.4 GHz", 6), Some(2437.0));
        assert_eq!(freq_mhz("2.4 GHz", 13), Some(2472.0));
        assert_eq!(freq_mhz("2.4 GHz", 14), Some(2484.0));
        assert_eq!(freq_mhz("2.4 GHz", 15), None);
        assert_eq!(freq_mhz("5 GHz", 36), Some(5180.0));
        assert_eq!(freq_mhz("5 GHz", 149), Some(5745.0));
        assert_eq!(freq_mhz("6 GHz", 1), Some(5955.0));
        assert_eq!(freq_mhz("6 GHz", 233), Some(7115.0));
        assert_eq!(freq_mhz("6 GHz", 234), None);
        assert_eq!(freq_mhz("60 GHz", 1), None);
    }
}
