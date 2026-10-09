//! 网卡 provider:GetIfTable2(统计/速率)+ GetAdaptersAddresses(静态识别)
//! + WLAN API(WiFi 关联细节)+ WMI(网卡驱动信息)。
//!
//! 设备清单在 probe 时固定;refresh 按 InterfaceIndex 回查新表,
//! 设备消失时把动态值标记为 ERROR(不崩溃、不重建清单)。

#![cfg(target_os = "windows")]

use crate::model::{
    Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType,
};
use crate::traits::Provider;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetAdaptersAddresses, GetIfTable2, GAA_FLAG_INCLUDE_ALL_INTERFACES,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

/// 802.11 接口的介质类型(IF_TYPE_IEEE80211)。
const IF_TYPE_IEEE80211: u32 = 71;

crate::wmi_struct!(WmiNetAdapter, "Win32_NetworkAdapter", {
    GUID: String,
    PNPDeviceID: String
});
crate::wmi_struct!(WmiSignedDriver, "Win32_PnPSignedDriver", {
    DeviceID: String,
    DriverVersion: String,
    DriverProviderName: String,
    InfName: String
});

/// 网卡驱动信息(wmi_struct 的字段是逐项 Option,这里收口成完整结构)。
#[derive(Clone, Default)]
struct NicDriverInfo {
    provider: String,
    version: String,
    inf: String,
}

struct Sample {
    rx_bytes: u64,
    tx_bytes: u64,
    at: Instant,
}

pub struct NetProvider {
    /// if_index → 上一次采样(算速率)
    prev: Mutex<HashMap<u32, Sample>>,
    /// WLAN 会话(系统无 WLAN 服务时为 None,此时不声明 WiFi 扩展传感器)。
    /// 句柄不做并发保证,所有查询都在此锁下串行执行。
    wlan: Mutex<Option<super::wlan::Wlan>>,
}

impl NetProvider {
    pub fn new() -> NetProvider {
        NetProvider {
            prev: Mutex::new(HashMap::new()),
            wlan: Mutex::new(super::wlan::Wlan::open()),
        }
    }
}

/// 枚举 GetIfTable2,返回 (if_index, row 快照)。
fn if_rows() -> Result<Vec<MIB_IF_ROW2>, String> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let status = GetIfTable2(&mut table);
        if status.0 != 0 {
            return Err(format!("GetIfTable2 失败: 0x{:08X}", status.0));
        }
        let mut rows = Vec::with_capacity((*table).NumEntries as usize);
        // windows-rs 中 Table 是 [MIB_IF_ROW2; 1](C 灵活数组成员),按指针遍历
        let base = (*table).Table.as_ptr();
        for i in 0..(*table).NumEntries as usize {
            rows.push(*base.add(i));
        }
        FreeMibTable(table as *const _);
        Ok(rows)
    }
}

/// 物理网卡才展示(过滤回环/隧道/软件设备)。
/// 6=Ethernet, 71=802.11 WiFi, 237=6LoWPAN。
fn is_physical(row: &MIB_IF_ROW2) -> bool {
    if !matches!(row.Type, 6 | 71 | 237) {
        return false;
    }
    let descr = wc_str(&row.Description);
    let alias = wc_str(&row.Alias);
    let lower = format!("{descr} {alias}").to_lowercase();
    // 常见软件/诊断虚拟网卡与 NDIS 过滤驱动(它们会复制物理网卡的统计)
    !lower.contains("filter")
        && !lower.contains("scheduler")
        && !lower.contains("wan miniport")
        && !lower.contains("kernel debug")
        && !lower.contains("wi-fi direct")
        && !lower.contains("virtual")
        && !lower.contains("microsoft wi-fi")
        && !lower.contains("bluetooth")
        && !lower.contains("tap-")
        && !lower.contains("wintun")
}

fn wc_str(ws: &[u16]) -> String {
    let end = ws.iter().position(|&c| c == 0).unwrap_or(ws.len());
    String::from_utf16_lossy(&ws[..end])
}

impl Provider for NetProvider {
    fn describe(&self) -> &str {
        "win-net"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let rows = if_rows()?;
        let wlan = self.wlan.lock().unwrap_or_else(|e| e.into_inner());
        let drv_map = nic_driver_map();
        let mut devices = Vec::new();
        for row in &rows {
            if !is_physical(row) {
                continue;
            }
            let idx = row.InterfaceIndex;
            let alias = wc_str(&row.Alias);
            let descr = wc_str(&row.Description);
            let mac: Vec<String> = row
                .PhysicalAddress
                .iter()
                .take(row.PhysicalAddressLength as usize)
                .map(|b| format!("{b:02X}"))
                .collect();
            let mac = mac.join(":");

            let mut b = DeviceBuilder::new(
                format!("net.if.{idx}"),
                if alias.is_empty() { descr.clone() } else { alias.clone() },
            )
            .vendor("Network Adapter")
            .model(descr.clone())
            .serial(if mac.is_empty() { format!("idx-{idx}") } else { mac.clone() });

            if !mac.is_empty() {
                b = b.static_str("mac", "MAC Address", mac);
            }
            b = b.static_str("interface_type", "Interface Type", format!("type-{}", row.Type));
            b = b.static_f64("mtu", "MTU", "bytes", row.Mtu as f64);
            b = b.static_str(
                "connection_name",
                "Connection Name",
                if alias.is_empty() { descr.clone() } else { alias },
            );

            // 驱动信息:网卡没有 Win32_VideoController 那样的单次查询字段,
            // 须经 Win32_NetworkAdapter 的 GUID 关联到 Win32_PnPSignedDriver。
            if let Some(info) = drv_map.get(&guid_key(&row.InterfaceGuid)) {
                if !info.provider.is_empty() {
                    b = b.static_str("driver_provider", "Driver Provider", info.provider.clone());
                }
                if !info.version.is_empty() {
                    b = b.static_str("driver_version", "Driver Version", info.version.clone());
                }
                if !info.inf.is_empty() {
                    b = b.static_str("driver_inf", "Driver INF", info.inf.clone());
                }
            }

            // WiFi:同一接口 GUID 走 WLAN API 补充关联细节与静态能力。
            // 只认 WLAN 站点接口:Wi-Fi Direct 之类的虚拟适配器也是 802.11
            // 介质类型,但查不到任何 WiFi 数据,声明了只会留一排错误的键。
            if row.Type == IF_TYPE_IEEE80211 {
                if let Some(w) = wlan.as_ref().filter(|w| w.has_interface(&row.InterfaceGuid)) {
                    let phys = w.supported_phys(&row.InterfaceGuid);
                    if !phys.is_empty() {
                        b = b.static_str(
                            "wifi_radio_types",
                            "Supported Radio Types",
                            phys.join(" "),
                        );
                    }
                    for s in WIFI_SENSORS {
                        b = b.dynamic(s.code, s.name, s.unit, s.vtype);
                    }
                }
            }

            b = b.dynamic("link_up", "Link Up", "", ValueType::Bool)
                .dynamic("link_speed_mbps", "Link Speed", "Mbps", ValueType::F64)
                .dynamic("rx_total_mb", "Received Total", "MB", ValueType::F64)
                .dynamic("tx_total_mb", "Sent Total", "MB", ValueType::F64)
                .dynamic("rx_kbps", "Download Rate", "KB/s", ValueType::F64)
                .dynamic("tx_kbps", "Upload Rate", "KB/s", ValueType::F64)
                .dynamic("rx_errors", "Receive Errors", "", ValueType::U64)
                .dynamic("tx_errors", "Transmit Errors", "", ValueType::U64);
            devices.push(b.build());
        }
        if devices.is_empty() {
            return Err("未发现物理网卡".into());
        }
        Ok(devices)
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let rows = match if_rows() {
            Ok(r) => r,
            Err(e) => {
                crate::logbuf::warn(format!("net refresh: {e}"));
                for d in devices.iter_mut() {
                    for s in d.sensors.iter_mut() {
                        if s.desc.kind == crate::model::SensorKind::Dynamic {
                            // 按各传感器自身的类型标错,不能统一写 F64
                            s.current = Reading::failed(Status::Error, s.desc.vtype);
                        }
                    }
                }
                return;
            }
        };
        let by_index: HashMap<u32, &MIB_IF_ROW2> =
            rows.iter().map(|r| (r.InterfaceIndex, r)).collect();

        let mut prev = self.prev.lock().unwrap_or_else(|e| e.into_inner());
        let wlan = self.wlan.lock().unwrap_or_else(|e| e.into_inner());
        for dev in devices.iter_mut() {
            // meta.id = "net.if.{idx}"
            let Some(idx_str) = dev.meta.id.rsplit('.').next() else {
                continue;
            };
            let Some(idx) = idx_str.parse::<u32>().ok() else {
                continue;
            };
            let Some(row) = by_index.get(&idx) else {
                // 接口消失:全部动态传感器标 Error(按各自类型),不残留旧 Ok 值
                for s in dev.sensors.iter_mut() {
                    if s.desc.kind == crate::model::SensorKind::Dynamic {
                        s.current = Reading::failed(Status::Error, s.desc.vtype);
                    }
                }
                continue;
            };

            let now = Instant::now();
            if row.Type == IF_TYPE_IEEE80211 {
                refresh_wifi(wlan.as_ref(), row, dev);
            }
            let sample = Sample {
                rx_bytes: row.InOctets,
                tx_bytes: row.OutOctets,
                at: now,
            };
            let (rx_kbps, tx_kbps) = {
                let old = prev.insert(idx, sample);
                match old {
                    Some(o) if o.rx_bytes <= row.InOctets => {
                        let dt = now.duration_since(o.at).as_secs_f64();
                        if dt > 0.05 {
                            (
                                (row.InOctets - o.rx_bytes) as f64 / dt / 1024.0,
                                (row.OutOctets - o.tx_bytes) as f64 / dt / 1024.0,
                            )
                        } else {
                            (-1.0, -1.0) // 间隔过短,本轮跳过
                        }
                    }
                    _ => (-1.0, -1.0), // 首轮等待
                }
            };
            if rx_kbps >= 0.0 {
                dev.set_reading("rx_kbps", Reading::now(Value::F64((rx_kbps * 10.0).round() / 10.0)));
                dev.set_reading("tx_kbps", Reading::now(Value::F64((tx_kbps * 10.0).round() / 10.0)));
            }
            dev.set_reading(
                "link_up",
                Reading::now(Value::Bool(row.OperStatus == IfOperStatusUp)),
            );
            dev.set_reading(
                "link_speed_mbps",
                Reading::now(Value::F64(row.TransmitLinkSpeed as f64 / 1_000_000.0)),
            );
            dev.set_reading(
                "rx_total_mb",
                Reading::now(Value::F64(row.InOctets as f64 / 1024.0 / 1024.0)),
            );
            dev.set_reading(
                "tx_total_mb",
                Reading::now(Value::F64(row.OutOctets as f64 / 1024.0 / 1024.0)),
            );
            dev.set_reading("rx_errors", Reading::now(Value::U64(row.InErrors)));
            dev.set_reading("tx_errors", Reading::now(Value::U64(row.OutErrors)));
        }
    }
}

// ---------------- WiFi 扩展 ----------------

/// WiFi 扩展传感器(仅 802.11 接口声明)。清单在 probe 固定,
/// 断连或读不到时按状态标非 OK,而不是让键消失。
struct WifiSensor {
    code: &'static str,
    name: &'static str,
    unit: &'static str,
    vtype: ValueType,
}

const WIFI_SENSORS: &[WifiSensor] = &[
    WifiSensor { code: "wifi_state", name: "WiFi State", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_ssid", name: "SSID", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_bssid", name: "BSSID", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_profile", name: "Profile", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_band", name: "Band", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_channel", name: "Channel", unit: "", vtype: ValueType::U64 },
    WifiSensor { code: "wifi_freq_mhz", name: "Frequency", unit: "MHz", vtype: ValueType::F64 },
    WifiSensor { code: "wifi_phy", name: "Radio Type", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_signal_pct", name: "Signal", unit: "%", vtype: ValueType::F64 },
    WifiSensor { code: "wifi_rssi_dbm", name: "RSSI", unit: "dBm", vtype: ValueType::F64 },
    WifiSensor { code: "wifi_rx_mbps", name: "Receive Rate", unit: "Mbps", vtype: ValueType::F64 },
    WifiSensor { code: "wifi_tx_mbps", name: "Transmit Rate", unit: "Mbps", vtype: ValueType::F64 },
    WifiSensor { code: "wifi_auth", name: "Authentication", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_cipher", name: "Cipher", unit: "", vtype: ValueType::Str },
    WifiSensor { code: "wifi_radio_on", name: "Radio On", unit: "", vtype: ValueType::Bool },
];

/// 刷新 WiFi 扩展键。
///
/// 状态语义:接口未关联(Disconnected/Associating…)时关联类字段本就无值,
/// 标 NotSupported;只有查询本身失败才标 Error —— 否则"没连 WiFi"会看起来
/// 像传感器故障。
fn refresh_wifi(wlan: Option<&super::wlan::Wlan>, row: &MIB_IF_ROW2, dev: &mut DeviceData) {
    let Some(d) = wlan.and_then(|w| w.query(&row.InterfaceGuid)) else {
        // 查询失败(接口刚消失 / WLAN 服务异常):整组含状态一律 Error
        mark_wifi(dev, Status::Error);
        return;
    };

    // 先全标"未关联不提供",再逐项覆盖;wifi_state 总有值,最后写
    mark_wifi(dev, Status::NotSupported);
    dev.set_reading("wifi_state", Reading::now(Value::Str(d.state.to_string())));
    if !d.connected {
        return;
    }

    // 频段要在搬走 String 字段之前算(它借的是整个 d)
    let band = d.band().unwrap_or_default().to_string();
    set_str(dev, "wifi_ssid", d.ssid);
    set_str(dev, "wifi_bssid", d.bssid);
    set_str(dev, "wifi_profile", d.profile);
    // "Unknown" 是 WLAN API 的枚举缺省值,等同于没有值
    set_str(dev, "wifi_band", band);
    set_str(dev, "wifi_phy", static_or_empty(d.phy));
    set_str(dev, "wifi_auth", static_or_empty(d.auth));
    set_str(dev, "wifi_cipher", static_or_empty(d.cipher));
    set_u64(dev, "wifi_channel", d.channel.map(|c| c as u64));
    set_f64(dev, "wifi_freq_mhz", d.freq_khz.map(|k| k as f64 / 1000.0));
    set_f64(dev, "wifi_signal_pct", d.signal_pct.map(|v| v as f64));
    set_f64(dev, "wifi_rssi_dbm", d.rssi_dbm.map(|v| v as f64));
    // WLAN API 的速率字段单位是 kbps,换算成 Mbps 展示
    set_f64(dev, "wifi_rx_mbps", d.rx_kbps.map(|v| v as f64 / 1000.0));
    set_f64(dev, "wifi_tx_mbps", d.tx_kbps.map(|v| v as f64 / 1000.0));
    // 射频开关取整卡:硬件开关(物理开关/BIOS 限制)与软件开关(飞行模式)
    set_bool(
        dev,
        "wifi_radio_on",
        d.radio_hw_on.zip(d.radio_sw_on).map(|(h, s)| h && s),
    );
}

/// 把 wifi_* 动态键标为给定状态(含 wifi_state,调用方在需要时覆盖它)。
fn mark_wifi(dev: &mut DeviceData, status: Status) {
    for s in dev.sensors.iter_mut() {
        if s.desc.code.starts_with("wifi_") && s.desc.kind == crate::model::SensorKind::Dynamic {
            s.current = Reading::failed(status, s.desc.vtype);
        }
    }
}

fn static_or_empty(s: &'static str) -> String {
    if s == "Unknown" { String::new() } else { s.to_string() }
}

// 下面几个 setter 只在有值时才写:没写到的键保留 mark_wifi 落的非 OK 状态。
fn set_str(dev: &mut DeviceData, code: &str, v: String) {
    if !v.is_empty() {
        dev.set_reading(code, Reading::now(Value::Str(v)));
    }
}

fn set_f64(dev: &mut DeviceData, code: &str, v: Option<f64>) {
    if let Some(x) = v {
        dev.set_reading(code, Reading::now(Value::F64(x)));
    }
}

fn set_u64(dev: &mut DeviceData, code: &str, v: Option<u64>) {
    if let Some(x) = v {
        dev.set_reading(code, Reading::now(Value::U64(x)));
    }
}

fn set_bool(dev: &mut DeviceData, code: &str, v: Option<bool>) {
    if let Some(x) = v {
        dev.set_reading(code, Reading::now(Value::Bool(x)));
    }
}

// ---------------- 驱动信息(WMI) ----------------

/// GUID → 比较用的键:大写、去花括号。Win32_NetworkAdapter.GUID 带花括号,
/// windows-rs 的 GUID Debug 输出不带,规格统一后再比。
fn guid_key(g: &windows::core::GUID) -> String {
    normalize_guid(&format!("{g:?}"))
}

fn normalize_guid(s: &str) -> String {
    s.trim()
        .trim_matches(|c| c == '{' || c == '}')
        .to_uppercase()
}

/// 网卡 GUID → 驱动信息。两步关联:Win32_NetworkAdapter 给
/// GUID↔PNPDeviceID,Win32_PnPSignedDriver 给 PNPDeviceID↔驱动版本。
fn nic_driver_map() -> HashMap<String, NicDriverInfo> {
    let Ok(conn) = super::shared_wmi() else {
        return HashMap::new();
    };
    let drivers: HashMap<String, NicDriverInfo> = conn
        .raw_query::<WmiSignedDriver>(
            "SELECT DeviceID, DriverVersion, DriverProviderName, InfName FROM Win32_PnPSignedDriver",
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|d| {
            Some((
                d.DeviceID?.to_uppercase(),
                NicDriverInfo {
                    provider: d.DriverProviderName.unwrap_or_default(),
                    version: d.DriverVersion.unwrap_or_default(),
                    inf: d.InfName.unwrap_or_default(),
                },
            ))
        })
        .collect();
    if drivers.is_empty() {
        return HashMap::new();
    }
    conn.raw_query::<WmiNetAdapter>("SELECT GUID, PNPDeviceID FROM Win32_NetworkAdapter")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|a| {
            let pnp = a.PNPDeviceID?;
            let info = drivers.get(&pnp.to_uppercase())?.clone();
            Some((normalize_guid(&a.GUID?), info))
        })
        .collect()
}

/// GetAdaptersAddresses 的辅助枚举(供未来扩展 IP/DNS 等静态信息)。
#[allow(dead_code)]
fn adapter_addresses() -> Vec<IP_ADAPTER_ADDRESSES_LH> {
    unsafe {
        let mut size = 32768u32;
        let mut buf = vec![0u8; size as usize];
        for _ in 0..3 {
            let status = GetAdaptersAddresses(
                0, // AF_UNSPEC
                GAA_FLAG_INCLUDE_ALL_INTERFACES,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            );
            if status == 0 {
                break;
            }
            if status != 111 /* ERROR_BUFFER_OVERFLOW */ {
                return Vec::new();
            }
            buf = vec![0u8; size as usize];
        }
        let mut out = Vec::new();
        let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !p.is_null() {
            out.push(*p);
            p = (*p).Next;
        }
        out
    }
}
