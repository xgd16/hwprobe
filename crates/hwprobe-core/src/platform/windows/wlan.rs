//! WiFi 关联细节:WLAN API(wlanapi.dll,系统自带、用户态免管理员)。
//!
//! 相当于 `netsh wlan show interfaces` 的进程内实现:接口状态、关联的
//! SSID/BSSID/配置文件、频段与信道、PHY 类型(802.11ax 等)、信号质量与
//! RSSI、收发协商速率(kbps)、认证与加密算法、射频硬/软开关,以及网卡
//! 支持的 PHY 类型集合(WlanGetInterfaceCapability,静态能力)。
//!
//! 频段只由 BSS 条目的中心频率判定,不用信道号:6 GHz 的信道号(1..233)
//! 与 2.4/5 GHz 重叠,单凭信道号无法区分 —— 频率是唯一无歧义的依据。
//!
//! 未连接时不编造字段:只给状态与射频开关,其余由上层标"错误"。

#![cfg(target_os = "windows")]

use windows::core::GUID;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::*;

/// WLAN 客户端会话(WlanOpenHandle)。
///
/// 句柄不做并发保证,调用方须串行化(NetProvider 用 Mutex 持有)。
/// HANDLE 是裸指针包装,此处声明可跨线程移动:调用始终在同一把锁下进行。
pub struct Wlan {
    handle: HANDLE,
}

unsafe impl Send for Wlan {}

/// 一次查询得到的关联细节。未连接时除 state/radio 外均为空。
#[derive(Default, Debug, Clone)]
pub struct WifiDetails {
    /// 接口状态名(wlan_interface_state_*)。
    pub state: &'static str,
    pub connected: bool,
    /// 硬件射频开关(网卡物理开关/BIOS)。
    pub radio_hw_on: Option<bool>,
    /// 软件射频开关(飞行模式)。
    pub radio_sw_on: Option<bool>,
    pub ssid: String,
    pub bssid: String,
    pub profile: String,
    pub phy: &'static str,
    pub signal_pct: Option<u32>,
    pub rssi_dbm: Option<i32>,
    /// 协商接收速率(kbps —— WLAN API 的 ulRxRate 就是这个单位)。
    pub rx_kbps: Option<u32>,
    /// 协商发送速率(kbps)。
    pub tx_kbps: Option<u32>,
    pub auth: &'static str,
    pub cipher: &'static str,
    pub channel: Option<u32>,
    /// 中心频率(kHz),来自关联 BSS 条目。
    pub freq_khz: Option<u32>,
}

impl WifiDetails {
    /// 中心频率 → 频段名。频率未知时返回 None(不猜)。
    pub fn band(&self) -> Option<&'static str> {
        band_from_khz(self.freq_khz?)
    }
}

impl Wlan {
    /// 打开 WLAN 客户端会话(协商版本 2 = Vista+ 接口)。
    /// WLAN 服务未运行时返回 None。
    pub fn open() -> Option<Wlan> {
        unsafe {
            let mut negotiated = 0u32;
            let mut handle = HANDLE::default();
            let rc = WlanOpenHandle(2, None, &mut negotiated, &mut handle);
            if rc != 0 {
                return None;
            }
            Some(Wlan { handle })
        }
    }

    /// 该 GUID 是不是 WLAN 站点接口(WlanEnumInterfaces 的成员)。
    ///
    /// Wi-Fi Direct / 承载网络等虚拟适配器在 GetIfTable2 里同样是 802.11
    /// 介质类型,但不在 WLAN 接口列表里,对其调用 WlanQueryInterface 一律失败。
    /// 上层据此决定要不要声明 WiFi 扩展键,免得虚拟适配器上挂一排错误的键。
    pub fn has_interface(&self, guid: &GUID) -> bool {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.handle, None, &mut list) != 0 || list.is_null() {
                return false;
            }
            let n = (*list).dwNumberOfItems as usize;
            let base = (*list).InterfaceInfo.as_ptr();
            let mut found = false;
            for i in 0..n {
                if (*base.add(i)).InterfaceGuid == *guid {
                    found = true;
                    break;
                }
            }
            WlanFreeMemory(list as *const _);
            found
        }
    }

    /// 查询当前关联细节。接口状态都读不到时返回 None。
    pub fn query(&self, guid: &GUID) -> Option<WifiDetails> {
        let state = self.query_struct::<WLAN_INTERFACE_STATE>(guid, wlan_intf_opcode_interface_state)?;
        let (radio_hw_on, radio_sw_on) = self.radio_state(guid);

        let mut d = WifiDetails {
            state: state_name(state),
            connected: state == wlan_interface_state_connected,
            radio_hw_on,
            radio_sw_on,
            ..Default::default()
        };
        if !d.connected {
            return Some(d);
        }

        let attrs = self.query_struct::<WLAN_CONNECTION_ATTRIBUTES>(
            guid,
            wlan_intf_opcode_current_connection,
        )?;
        let a = &attrs.wlanAssociationAttributes;
        let sec = &attrs.wlanSecurityAttributes;

        d.ssid = ssid_string(&a.dot11Ssid);
        d.bssid = bssid_string(&a.dot11Bssid);
        d.profile = utf16_field(&attrs.strProfileName);
        d.phy = phy_name(a.dot11PhyType);
        d.signal_pct = Some(a.wlanSignalQuality);
        d.rx_kbps = Some(a.ulRxRate);
        d.tx_kbps = Some(a.ulTxRate);
        d.auth = auth_name(sec.dot11AuthAlgorithm);
        d.cipher = cipher_name(sec.dot11CipherAlgorithm);
        d.channel = self.query_struct::<u32>(guid, wlan_intf_opcode_channel_number);

        // BSS 条目同时给中心频率和 RSSI:频率是频段判定的唯一依据,
        // RSSI 也比 opcode 更可靠(部分驱动不支持 wlan_intf_opcode_rssi)。
        if let Some((freq_khz, rssi)) = self.bss_entry(guid, &a.dot11Bssid) {
            d.freq_khz = Some(freq_khz);
            d.rssi_dbm = Some(rssi);
        } else {
            d.rssi_dbm = self.query_struct::<i32>(guid, wlan_intf_opcode_rssi);
        }
        Some(d)
    }

    /// 网卡支持的 PHY 类型(去重后的显示名,保持驱动的上报顺序)。
    pub fn supported_phys(&self, guid: &GUID) -> Vec<&'static str> {
        unsafe {
            let mut cap: *mut WLAN_INTERFACE_CAPABILITY = std::ptr::null_mut();
            let rc = WlanGetInterfaceCapability(self.handle, guid, None, &mut cap);
            if rc != 0 || cap.is_null() {
                return Vec::new();
            }
            let n = ((*cap).dwNumberOfSupportedPhys as usize).min((*cap).dot11PhyTypes.len());
            let mut out: Vec<&'static str> = Vec::new();
            for i in 0..n {
                let name = phy_name((*cap).dot11PhyTypes[i]);
                if name != "Unknown" && !out.contains(&name) {
                    out.push(name);
                }
            }
            WlanFreeMemory(cap as *const _);
            out
        }
    }

    /// 射频开关:(硬件, 软件)。读不到时都是 None。
    fn radio_state(&self, guid: &GUID) -> (Option<bool>, Option<bool>) {
        let Some(rs) =
            self.query_struct::<WLAN_RADIO_STATE>(guid, wlan_intf_opcode_radio_state)
        else {
            return (None, None);
        };
        if rs.dwNumberOfPhys == 0 {
            return (None, None);
        }
        // 多 PHY 网卡取第 0 个:射频开关在 Windows 里是整卡级别的
        let p = &rs.PhyRadioState[0];
        (
            Some(p.dot11HardwareRadioState == dot11_radio_state_on),
            Some(p.dot11SoftwareRadioState == dot11_radio_state_on),
        )
    }

    /// 在扫描表里找当前关联的 BSS,返回 (中心频率 kHz, RSSI dBm)。
    fn bss_entry(&self, guid: &GUID, bssid: &[u8; 6]) -> Option<(u32, i32)> {
        unsafe {
            let mut list: *mut WLAN_BSS_LIST = std::ptr::null_mut();
            let rc = WlanGetNetworkBssList(
                self.handle,
                guid,
                None,
                dot11_BSS_type_infrastructure,
                false,
                None,
                &mut list,
            );
            if rc != 0 || list.is_null() {
                return None;
            }
            let n = (*list).dwNumberOfItems as usize;
            let base = (*list).wlanBssEntries.as_ptr();
            let mut found = None;
            for i in 0..n {
                let e = &*base.add(i);
                if e.dot11Bssid == *bssid {
                    found = Some((e.ulChCenterFrequency, e.lRssi));
                    break;
                }
            }
            WlanFreeMemory(list as *const _);
            found
        }
    }

    /// 查询返回 POD 的 opcode:整块拷出来再交还 WLAN 分配的内存。
    fn query_struct<T: Copy>(&self, guid: &GUID, opcode: WLAN_INTF_OPCODE) -> Option<T> {
        unsafe {
            let mut size = 0u32;
            let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            let rc = WlanQueryInterface(
                self.handle,
                guid,
                opcode,
                None,
                &mut size,
                &mut ptr,
                None,
            );
            if rc != 0 || ptr.is_null() || (size as usize) < std::mem::size_of::<T>() {
                if !ptr.is_null() {
                    WlanFreeMemory(ptr);
                }
                return None;
            }
            let value = std::ptr::read_unaligned(ptr as *const T);
            WlanFreeMemory(ptr);
            Some(value)
        }
    }
}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            let _ = WlanCloseHandle(self.handle, None);
        }
    }
}

// ---------------- 字段解码 ----------------

/// 定长 UTF-16 字段(strProfileName / strInterfaceDescription)转字符串。
fn utf16_field(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

/// SSID 是长度前缀的字节串(UTF-8 语义,但不保证合法)。
fn ssid_string(ssid: &DOT11_SSID) -> String {
    let n = (ssid.uSSIDLength as usize).min(ssid.ucSSID.len());
    String::from_utf8_lossy(&ssid.ucSSID[..n]).to_string()
}

fn bssid_string(b: &[u8; 6]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

type Khz = u32;

/// 中心频率(kHz)→ 频段名。边界取自各频段的实际信道范围。
fn band_from_khz(khz: u32) -> Option<&'static str> {
    let mhz = khz as f64 / 1000.0;
    match mhz {
        m if (2400.0..2500.0).contains(&m) => Some("2.4 GHz"),
        m if (4900.0..5925.0).contains(&m) => Some("5 GHz"),
        m if (5925.0..7125.0).contains(&m) => Some("6 GHz"),
        m if (57000.0..71000.0).contains(&m) => Some("60 GHz"),
        _ => None,
    }
}

/// DOT11_PHY_TYPE → 用户可见的制式名(dot11_phy_type_*)。
fn phy_name(t: DOT11_PHY_TYPE) -> &'static str {
    match t.0 {
        1 => "FHSS",
        2 => "802.11b",
        3 => "IrDA",
        4 => "802.11a",
        5 | 6 => "802.11g",
        7 => "802.11n",
        8 => "802.11ac",
        9 => "802.11ad",
        10 => "802.11ax",
        11 => "802.11be",
        _ => "Unknown",
    }
}

/// WLAN_INTERFACE_STATE → 状态名。
fn state_name(s: WLAN_INTERFACE_STATE) -> &'static str {
    if s == wlan_interface_state_connected {
        "Connected"
    } else if s == wlan_interface_state_disconnected {
        "Disconnected"
    } else if s == wlan_interface_state_associating {
        "Associating"
    } else if s == wlan_interface_state_authenticating {
        "Authenticating"
    } else if s == wlan_interface_state_disconnecting {
        "Disconnecting"
    } else if s == wlan_interface_state_discovering {
        "Discovering"
    } else if s == wlan_interface_state_ad_hoc_network_formed {
        "Ad-hoc"
    } else if s == wlan_interface_state_not_ready {
        "Not Ready"
    } else {
        "Unknown"
    }
}

/// DOT11_AUTH_ALGORITHM → 认证方式名。枚举在 SDK 里是显式赋值不是连续序号,
/// 故一律用 windows crate 的常量比较。8/11 两个值都对应 WPA3-Enterprise
/// (SDK 里 DOT11_AUTH_ALGO_WPA3_ENT_192 与 DOT11_AUTH_ALGO_WPA3 同值)。
fn auth_name(a: DOT11_AUTH_ALGORITHM) -> &'static str {
    if a == DOT11_AUTH_ALGO_80211_OPEN {
        "Open"
    } else if a == DOT11_AUTH_ALGO_80211_SHARED_KEY {
        "Shared Key"
    } else if a == DOT11_AUTH_ALGO_WPA {
        "WPA-Enterprise"
    } else if a == DOT11_AUTH_ALGO_WPA_PSK {
        "WPA-Personal"
    } else if a == DOT11_AUTH_ALGO_WPA_NONE {
        "WPA-None"
    } else if a == DOT11_AUTH_ALGO_RSNA {
        "WPA2-Enterprise"
    } else if a == DOT11_AUTH_ALGO_RSNA_PSK {
        "WPA2-Personal"
    } else if a == DOT11_AUTH_ALGO_WPA3_SAE {
        "WPA3-Personal"
    } else if a == DOT11_AUTH_ALGO_WPA3_ENT {
        "WPA3-Enterprise 192"
    } else if a == DOT11_AUTH_ALGO_WPA3 {
        "WPA3-Enterprise"
    } else if a == DOT11_AUTH_ALGO_OWE {
        "OWE"
    } else {
        "Unknown"
    }
}

/// DOT11_CIPHER_ALGORITHM → 加密算法名(同样是显式赋值:CCMP=4、WEP104=5,
/// 3 号位空缺;按序号硬编会整体错位)。
fn cipher_name(c: DOT11_CIPHER_ALGORITHM) -> &'static str {
    if c == DOT11_CIPHER_ALGO_NONE {
        "None"
    } else if c == DOT11_CIPHER_ALGO_WEP40 {
        "WEP-40"
    } else if c == DOT11_CIPHER_ALGO_TKIP {
        "TKIP"
    } else if c == DOT11_CIPHER_ALGO_CCMP {
        "CCMP"
    } else if c == DOT11_CIPHER_ALGO_WEP104 {
        "WEP-104"
    } else if c == DOT11_CIPHER_ALGO_BIP {
        "BIP"
    } else if c == DOT11_CIPHER_ALGO_GCMP {
        "GCMP"
    } else if c == DOT11_CIPHER_ALGO_GCMP_256 {
        "GCMP-256"
    } else if c == DOT11_CIPHER_ALGO_CCMP_256 {
        "CCMP-256"
    } else if c == DOT11_CIPHER_ALGO_BIP_GMAC_128 {
        "BIP-GMAC-128"
    } else if c == DOT11_CIPHER_ALGO_BIP_GMAC_256 {
        "BIP-GMAC-256"
    } else if c == DOT11_CIPHER_ALGO_BIP_CMAC_256 {
        "BIP-CMAC-256"
    } else if c == DOT11_CIPHER_ALGO_WEP {
        "WEP"
    } else if c == DOT11_CIPHER_ALGO_WPA_USE_GROUP {
        "Use Group"
    } else {
        "Unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_boundaries() {
        assert_eq!(band_from_khz(2_412_000), Some("2.4 GHz"));
        assert_eq!(band_from_khz(2_484_000), Some("2.4 GHz"));
        assert_eq!(band_from_khz(5_180_000), Some("5 GHz"));
        assert_eq!(band_from_khz(5_885_000), Some("5 GHz"));
        assert_eq!(band_from_khz(5_955_000), Some("6 GHz"));
        assert_eq!(band_from_khz(7_115_000), Some("6 GHz"));
        assert_eq!(band_from_khz(60_480_000), Some("60 GHz"));
        assert_eq!(band_from_khz(0), None);
        assert_eq!(band_from_khz(900_000), None);
    }

    #[test]
    fn phy_and_cipher_names() {
        assert_eq!(phy_name(dot11_phy_type_eht), "802.11be");
        assert_eq!(phy_name(dot11_phy_type_he), "802.11ax");
        assert_eq!(phy_name(dot11_phy_type_dsss), "802.11b");
        assert_eq!(phy_name(dot11_phy_type_hrdsss), "802.11g");
        assert_eq!(phy_name(dot11_phy_type_unknown), "Unknown");
        assert_eq!(cipher_name(DOT11_CIPHER_ALGO_CCMP), "CCMP");
        assert_eq!(cipher_name(DOT11_CIPHER_ALGO_NONE), "None");
        assert_eq!(cipher_name(DOT11_CIPHER_ALGO_WEP104), "WEP-104");
        // 3 号位在枚举里空缺:按序号硬编会把它当成 CCMP
        assert_eq!(cipher_name(DOT11_CIPHER_ALGORITHM(3)), "Unknown");
        assert_eq!(auth_name(DOT11_AUTH_ALGO_WPA3_SAE), "WPA3-Personal");
        assert_eq!(auth_name(DOT11_AUTH_ALGO_OWE), "OWE");
        assert_eq!(auth_name(DOT11_AUTH_ALGO_RSNA_PSK), "WPA2-Personal");
    }

    #[test]
    fn ssid_is_length_prefixed() {
        let mut s = DOT11_SSID { uSSIDLength: 6, ucSSID: [0; 32] };
        s.ucSSID[..6].copy_from_slice(b"net_5g");
        s.ucSSID[6] = b'X'; // 长度之后的字节不属于 SSID
        assert_eq!(ssid_string(&s), "net_5g");

        let empty = DOT11_SSID { uSSIDLength: 0, ucSSID: [b'A'; 32] };
        assert_eq!(ssid_string(&empty), "");
    }

    #[test]
    fn utf16_field_stops_at_nul() {
        let mut buf = [0u16; 8];
        buf[..3].copy_from_slice(&[b'a' as u16, b'b' as u16, b'c' as u16]);
        assert_eq!(utf16_field(&buf), "abc");
        assert_eq!(utf16_field(&[0u16; 4]), "");
    }

    #[test]
    fn bssid_format() {
        assert_eq!(bssid_string(&[0xd0, 0x5b, 0xcb, 0xc5, 0x67, 0xa2]), "D0:5B:CB:C5:67:A2");
    }
}
