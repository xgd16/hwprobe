//! Network provider(macOS):NET_RT_IFLIST2 接口统计 + getifaddrs 链路层身份
//! + CoreWLAN WiFi 关联细节。
//!
//! 设备清单在 probe 时固定;refresh 按 if_index 回查新表,设备消失时把动态值
//! 标为 ERROR(不崩溃、不重建清单)—— 与 Windows 版 `windows/net.rs` 同构。
//!
//! 数据源与结构体都以本机 SDK 头文件为准(`$(xcrun --show-sdk-path)`),不凭记忆:
//! - `sys/socket.h`:`NET_RT_IFLIST2 = 6`(注意**不是**老 XNU 里的 5)、`PF_ROUTE = 17`;
//! - `sys/sysctl.h`:`CTL_NET = 4`;
//! - `net/route.h`:`RTM_IFINFO2 = 0x12`、`RTM_VERSION = 5`;
//! - `net/if.h`:`struct if_msghdr2`;`net/if_var.h`:`struct if_data64`;
//! - `net/if_dl.h`:`struct sockaddr_dl`;`net/if_types.h`:IFT_*。
//!
//! 一个与常见资料不符的事实:**这个 SDK 的 `if_data64` 里没有 `ifi_link_state`**
//! (整个 `net/` 头文件搜不到这个字段)。链路状态只能由 `ifm_flags` 的
//! `IFF_UP | IFF_RUNNING` 判定(即 `ifconfig` 显示 "status: active" 的同一判据)。
//!
//! 另一个实测事实:**getifaddrs / 路由 socket 的链路层地址被屏蔽成
//! `02:00:00:00:00:00`**,MAC 需要走 CoreWLAN / IOKit 兜底 —— 详见
//! [`resolve_mac`] 的说明。

use crate::model::{
    Category, DeviceBuilder, DeviceData, Reading, SensorKind, Status, Value, ValueType,
};
use crate::traits::Provider;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use io_kit_sys::types::io_registry_entry_t;
use io_kit_sys::{IOObjectRelease, IORegistryEntryCreateCFProperty, IORegistryEntryGetParentEntry};
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::wlan::Wlan;

// ---- SDK 常量(取值见文件头)----------------------------------------------

/// `net/if.h`:IFF_UP / IFF_RUNNING。
const IFF_UP: u32 = 0x1;
const IFF_RUNNING: u32 = 0x40;

/// `net/if_types.h`:IFT_ETHER = 0x6(以太网类;macOS 的 WiFi 口也报这个类型)。
const IFT_ETHER: u8 = 0x6;
/// `net/if_types.h` 里没有 IFT_IEEE80211;个别驱动/系统会报 IANA ifType 71,
/// 一并认掉,免得漏接口。
const IFT_IEEE80211: u8 = 71;

/// `net/route.h`:#define RTM_IFINFO2 0x12
const RTM_IFINFO2: u8 = 0x12;

// ---- SDK 结构体(字段顺序照头文件)----------------------------------------

/// `net/if_var.h` 的 `struct if_data64`。
///
/// 尾部的 `ifi_lastchange`(`IF_DATA_TIMEVAL`,LP64 下是 `timeval32`、ILP32 下是
/// `timeval`)不读就不声明:省掉它不影响前面字段的偏移,也免掉两种布局的差异。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct IfData64 {
    ifi_type: u8,
    ifi_typelen: u8,
    ifi_physical: u8,
    ifi_addrlen: u8,
    ifi_hdrlen: u8,
    ifi_recvquota: u8,
    ifi_xmitquota: u8,
    ifi_unused1: u8,
    ifi_mtu: u32,
    ifi_metric: u32,
    ifi_baudrate: u64,
    ifi_ipackets: u64,
    ifi_ierrors: u64,
    ifi_opackets: u64,
    ifi_oerrors: u64,
    ifi_collisions: u64,
    ifi_ibytes: u64,
    ifi_obytes: u64,
    ifi_imcasts: u64,
    ifi_omcasts: u64,
    ifi_iqdrops: u64,
    ifi_noproto: u64,
    ifi_recvtiming: u32,
    ifi_xmittiming: u32,
}

/// `net/if.h` 的 `struct if_msghdr2`(内含 `struct if_data64 ifm_data`)。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct IfMsghdr2 {
    ifm_msglen: u16,
    ifm_version: u8,
    ifm_type: u8,
    ifm_addrs: i32,
    ifm_flags: i32,
    ifm_index: u16,
    ifm_snd_len: i32,
    ifm_snd_maxlen: i32,
    ifm_snd_drops: i32,
    ifm_timer: i32,
    ifm_data: IfData64,
}

// ---- 诊断用枚举结果(探针会打印)------------------------------------------

/// NET_RT_IFLIST2 里的一行接口统计。
#[derive(Clone, Debug)]
pub struct IfRow {
    pub index: u32,
    pub flags: u32,
    pub mtu: u32,
    /// 链路速率(位/秒,`ifi_baudrate` 就是这个单位)。
    pub baudrate: u64,
    pub ibytes: u64,
    pub obytes: u64,
    pub ipackets: u64,
    pub opackets: u64,
    pub ierrors: u64,
    pub oerrors: u64,
}

impl IfRow {
    /// 链路是否连通。SDK 的 `if_data64` 没有 `ifi_link_state` 字段,只能看
    /// `IFF_UP` 且 `IFF_RUNNING`(即 `ifconfig` 的 "status: active" 判据)。
    pub fn link_up(&self) -> bool {
        self.flags & (IFF_UP | IFF_RUNNING) == (IFF_UP | IFF_RUNNING)
    }
}

/// `getifaddrs` 的 AF_LINK 行:接口名、if_index、链路层类型、MAC。
#[derive(Clone, Debug)]
pub struct LinkAddr {
    pub name: String,
    pub index: u32,
    /// `net/if_types.h` 的 IFT_*。
    pub sdl_type: u8,
    /// 大写冒号分隔;取不到链路层地址时为空串。
    pub mac: String,
}

// ---- 数据源 ---------------------------------------------------------------

/// `sysctl(CTL_NET, PF_ROUTE, 0, 0, NET_RT_IFLIST2, 0)`:接口清单 + 64 位统计。
///
/// 返回缓冲区是一串路由消息,每条以 `msglen/version/type` 开头
/// (rt_msghdr / if_msghdr / if_msghdr2 / ifa_msghdr 的头 4 字节布局相同),
/// 所以按首部 msglen 逐条推进即可。这里只认 RTM_IFINFO2(`if_msghdr2`,
/// 带 `if_data64` 统计);RTM_NEWADDR 之类的地址消息直接跳过。
pub fn iflist() -> Result<Vec<IfRow>, String> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let mut len: libc::size_t = 0;
    // SAFETY: oldp 为 NULL 只问长度,不写内存。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(format!(
            "sysctl(NET_RT_IFLIST2) 取长度失败: {}",
            last_errno()
        ));
    }
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; len];
    // SAFETY: buf 容量就是上面问到的 len,与 oldlenp 声明一致,内核写满即返回。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(format!(
            "sysctl(NET_RT_IFLIST2) 取数据失败: {}",
            last_errno()
        ));
    }

    let total = len.min(buf.len());
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= total {
        // 消息头:msglen(u16,@0)、version(u8,@2)、type(u8,@3)。
        let msglen = u16::from_ne_bytes([buf[off], buf[off + 1]]) as usize;
        let msg_type = buf[off + 3];
        if msglen < 4 || off + msglen > total {
            break; // 内核给的缓冲区应当自洽;真不自洽就别死循环
        }
        if msg_type == RTM_IFINFO2 && msglen >= std::mem::size_of::<IfMsghdr2>() {
            // 整块拷贝而不是解引用:sysctl 缓冲区是 Vec<u8>,不保证 8 字节对齐。
            // SAFETY: off + size_of::<IfMsghdr2>() <= off + msglen <= total,读在界内。
            let m: IfMsghdr2 =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const IfMsghdr2) };
            out.push(IfRow {
                index: m.ifm_index as u32,
                flags: m.ifm_flags as u32,
                mtu: m.ifm_data.ifi_mtu,
                baudrate: m.ifm_data.ifi_baudrate,
                ibytes: m.ifm_data.ifi_ibytes,
                obytes: m.ifm_data.ifi_obytes,
                ipackets: m.ifm_data.ifi_ipackets,
                opackets: m.ifm_data.ifi_opackets,
                ierrors: m.ifm_data.ifi_ierrors,
                oerrors: m.ifm_data.ifi_oerrors,
            });
        }
        off += msglen;
    }
    Ok(out)
}

/// 接口身份(MAC / BSD 名 / 链路层类型)走 getifaddrs 的 AF_LINK 条目。
///
/// `sockaddr_dl` 里 `sdl_data` 的前 `sdl_nlen` 字节是 BSD 名,紧随其后的
/// `sdl_alen` 字节是链路层地址(即 BSD 的 `LLADDR(sdl)`)。头文件写明
/// `sdl_data[12]` 只是"最小工作区,可以更长",所以名与 MAC 的读取一律以
/// 内核给的 `sdl_len` 为界,不越出这条 sockaddr。
pub fn link_addrs() -> Vec<LinkAddr> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: 成功后 head 指向内核分配的链表,由 freeifaddrs 归还。
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        crate::logbuf::warn(format!("getifaddrs 失败: {}", last_errno()));
        return out;
    }
    let data_off = std::mem::offset_of!(libc::sockaddr_dl, sdl_data);
    let mut p = head;
    while !p.is_null() {
        // SAFETY: p 来自 getifaddrs 链表,每次迭代沿 ifa_next 前进到下一个节点。
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        if ifa.ifa_name.is_null() || ifa.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_addr 是 sockaddr,sa_family 与 sockaddr_dl 的 sdl_family 同位。
        if unsafe { (*ifa.ifa_addr).sa_family as i32 } != libc::AF_LINK {
            continue;
        }
        // SAFETY: 已确认 sa_family == AF_LINK,按 sockaddr_dl 解释安全。
        let sdl = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_dl>() };
        let total = sdl.sdl_len as usize;
        let nlen = sdl.sdl_nlen as usize;
        let alen = sdl.sdl_alen as usize;
        let data = sdl.sdl_data.as_ptr();
        let name = if nlen > 0 && data_off + nlen <= total {
            // SAFETY: 读 [data_off, data_off+nlen),已判在 sdl_len 内。
            // sdl_data 是 [c_char](arm64 上是 i8),按字节解释要转 u8。
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), nlen) };
            String::from_utf8_lossy(bytes).into_owned()
        } else {
            continue;
        };
        let mac = if alen > 0 && data_off + nlen + alen <= total {
            // SAFETY: 读 [data_off+nlen, data_off+nlen+alen),已判在 sdl_len 内。
            let bytes = unsafe { std::slice::from_raw_parts(data.add(nlen).cast::<u8>(), alen) };
            bytes
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join(":")
        } else {
            String::new()
        };
        out.push(LinkAddr {
            name,
            index: sdl.sdl_index as u32,
            sdl_type: sdl.sdl_type,
            mac,
        });
    }
    // SAFETY: head 是 getifaddrs 返回的链表头,只释放一次。
    unsafe { libc::freeifaddrs(head) };
    out
}

fn last_errno() -> String {
    std::io::Error::last_os_error().to_string()
}

// ---- MAC:三层来源 ---------------------------------------------------------
//
// 一个重要的现实(2026-10 在 macOS 27.2 上实测):**getifaddrs 与路由 socket 把
// 链路层地址屏蔽成了占位值 `02:00:00:00:00:00`**,每个接口都一样;而
// `ifconfig` / `netstat`(Apple 平台签名的程序)与 IOKit / CoreWLAN 仍能拿到真值。
// 把占位值当 MAC 报出去,既违反"不给假数据",又会让所有接口的 serial 撞车,
// 所以下面认得这个占位值并继续往下找真值。
const MAC_PLACEHOLDER: &str = "02:00:00:00:00:00";

/// 解析接口 MAC,来源按可靠性排序,取第一个确定值:
/// 1. getifaddrs 的 `sockaddr_dl`(指定来源;未被屏蔽的系统上就是真值);
/// 2. CoreWLAN 的 `hardwareAddress`(WiFi 站点接口,按 BSD 名精确对应);
/// 3. IOKit 父链上的 `IOMACAddress` —— **只在候选值彼此一致时才采信**
///    (候选不一致说明这条链上有多个不同地址,归属存疑,宁缺勿错)。
///
/// 实测对号(与 `networksetup -getmacaddress` / `system_profiler` 一致):
/// 本机 WiFi 口有**两个**地址 —— 硬件地址 5C:E9:1E:B5:09:9B(CoreWLAN /
/// `networksetup` 报这个)与私有随机地址 1A:AA:18:EE:16:81(`ifconfig` /
/// `netstat` 报这个,开了"私有 Wi-Fi 地址"就换)。CoreWLAN 这一层给的是
/// **硬件地址**;IOKit 父链上那两个候选正是这两个地址,互不相同,所以本规则
/// 对 WiFi 口会主动让路给第 2 层,对以太网口则照常采信唯一候选。
///
/// 三层都拿不到就返回 `None`:调用方不声明 `mac` 键,不填占位值。
pub fn resolve_mac(name: &str, sdl_mac: &str, wifi: bool, wlan: Option<&Wlan>) -> Option<String> {
    if !sdl_mac.is_empty() && sdl_mac != MAC_PLACEHOLDER {
        return Some(sdl_mac.to_string());
    }
    if wifi {
        if let Some(m) = wlan.and_then(|w| w.hardware_address(name)) {
            if !m.is_empty() && m != MAC_PLACEHOLDER {
                return Some(m.to_uppercase());
            }
        }
    }
    iokit_mac(name)
}

/// IOKit 侧的 MAC 候选:从 `IONetworkInterface`(按 BSD 名对号)出发沿
/// `IOService` 平面向上最多 4 层,收集沿途的 `IOMACAddress`。
/// 只有**全部候选值一致**时返回那个值,否则返回 `None`(归属存疑)。
fn iokit_mac(name: &str) -> Option<String> {
    use io_kit_sys::keys::kIOServicePlane;

    for obj in super::iokit::matching_services("IONetworkInterface") {
        let Some(bsd) = super::iokit::entry_prop(&obj, "BSD Name", super::iokit::cf_string) else {
            continue;
        };
        if bsd != name {
            continue;
        }
        let mut candidates: Vec<String> = Vec::new();
        // 第 0 层是接口对象本身(归 IoObject 管,不释放);父节点逐级自持自放。
        let mut cur: io_registry_entry_t = obj.raw();
        let mut owned: Option<io_registry_entry_t> = None;
        for hop in 0..5 {
            if hop > 0 {
                let mut parent: io_registry_entry_t = 0;
                // SAFETY: cur 是有效 registry 项;parent 出参按 Create 语义返回 +1。
                let kr = unsafe {
                    IORegistryEntryGetParentEntry(cur, kIOServicePlane, &mut parent)
                };
                if kr != 0 || parent == 0 {
                    break;
                }
                if let Some(prev) = owned.take() {
                    // SAFETY: prev 是上一轮 GetParentEntry 得来的 +1 引用。
                    unsafe { IOObjectRelease(prev) };
                }
                owned = Some(parent);
                cur = parent;
            }
            if let Some(m) = registry_mac(cur) {
                if !candidates.contains(&m) {
                    candidates.push(m);
                }
            }
        }
        if let Some(last) = owned {
            // SAFETY: 循环里最后一次 GetParentEntry 得来的 +1 引用。
            unsafe { IOObjectRelease(last) };
        }
        if candidates.len() == 1 {
            return candidates.pop();
        }
        return None; // 候选不一致(或一个都没有):不猜
    }
    None
}

/// 读 registry 项的 `IOMACAddress`(CFData,通常 6 字节)→ "AA:BB:CC:DD:EE:FF"。
fn registry_mac(entry: io_registry_entry_t) -> Option<String> {
    let key = CString::new("IOMACAddress").ok()?;
    // SAFETY: key 是合法 NUL 结尾字符串;失败返回 null。
    let ckey = unsafe {
        core_foundation_sys::string::CFStringCreateWithCString(
            std::ptr::null(),
            key.as_ptr(),
            core_foundation_sys::string::kCFStringEncodingUTF8,
        )
    };
    if ckey.is_null() {
        return None;
    }
    // SAFETY: entry 有效;Create 语义返回 +1 引用,下面负责释放。
    let v = unsafe { IORegistryEntryCreateCFProperty(entry, ckey, std::ptr::null(), 0) };
    // SAFETY: ckey 是上面 Create 出来的 CFString,只释放一次。
    unsafe { CFRelease(ckey as CFTypeRef) };
    if v.is_null() {
        return None;
    }
    let bytes = super::iokit::cf_data(v);
    // SAFETY: v 是 Create 语义返回的 +1 引用,拷出数据后释放。
    unsafe { CFRelease(v) };
    let b = bytes?;
    if b.len() < 6 {
        return None;
    }
    Some(b[..6].iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(":"))
}

/// 物理网卡才展示(对齐 Windows 版 `is_physical` 的过滤哲学:只留真实硬件口)。
///
/// macOS 上 BSD 约定内置/USB 网卡与 WiFi 都叫 `enN`;`lo0`(环回)、`utun*`
/// (VPN 隧道)、`awdl0`/`llw0`(WiFi Direct)、`ap1`(SoftAP)、`nan0`、
/// `bridge*`、`vmenet*`、`anpi*`(平台内部口)都不叫 `enN`,天然排除。
pub fn is_physical(name: &str, sdl_type: u8) -> bool {
    if !matches!(sdl_type, IFT_ETHER | IFT_IEEE80211) {
        return false;
    }
    match name.strip_prefix("en") {
        Some(rest) => !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

// ---- provider -------------------------------------------------------------

struct Sample {
    rx_bytes: u64,
    tx_bytes: u64,
    at: Instant,
}

pub struct NetProvider {
    /// if_index → 上一次采样(算速率)
    prev: Mutex<HashMap<u32, Sample>>,
    /// CoreWLAN 会话(无 WiFi 栈时为 None,此时不声明 WiFi 扩展传感器)。
    /// 句柄不做并发保证,所有查询都在此锁下串行执行。
    wlan: Mutex<Option<Wlan>>,
}

impl NetProvider {
    pub fn new() -> NetProvider {
        NetProvider {
            prev: Mutex::new(HashMap::new()),
            wlan: Mutex::new(Wlan::open()),
        }
    }
}

impl Provider for NetProvider {
    fn describe(&self) -> &str {
        "mac-net"
    }

    fn category(&self) -> Category {
        Category::Network
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let rows = iflist()?;
        let by_index: HashMap<u32, &IfRow> = rows.iter().map(|r| (r.index, r)).collect();
        let links = link_addrs();
        let wlan = self.wlan.lock().unwrap_or_else(|e| e.into_inner());
        let mut devices = Vec::new();
        for link in &links {
            if !is_physical(&link.name, link.sdl_type) {
                continue;
            }
            let idx = link.index;
            // WiFi:只认 CoreWLAN 认得的站点接口。虚拟 802.11 设备(Wi-Fi Direct
            // 之类)查不到任何 WiFi 数据,声明了只会留一排错误的键。
            let is_wifi = wlan.as_ref().is_some_and(|w| w.has_interface(&link.name));
            // MAC 走三层来源(见 resolve_mac);拿不到就不声明该键,不填占位值。
            let mac = resolve_mac(&link.name, &link.mac, is_wifi, wlan.as_ref());

            let mut b = DeviceBuilder::new(format!("net.if.{idx}"), link.name.clone()).serial(
                mac.clone().unwrap_or_else(|| format!("idx-{idx}")),
            );
            if let Some(m) = &mac {
                b = b.static_str("mac", "MAC Address", m.clone());
            }
            b = b.static_str(
                "interface_type",
                "Interface Type",
                format!("type-{}", link.sdl_type),
            );
            if let Some(row) = by_index.get(&idx) {
                b = b.static_f64("mtu", "MTU", "bytes", row.mtu as f64);
            }
            b = b.static_str("connection_name", "Connection Name", link.name.clone());

            if is_wifi {
                for s in WIFI_SENSORS {
                    b = b.dynamic(s.code, s.name, s.unit, s.vtype);
                }
            }

            b = b
                .dynamic("link_up", "Link Up", "", ValueType::Bool)
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
        let rows = match iflist() {
            Ok(r) => r,
            Err(e) => {
                crate::logbuf::warn(format!("net refresh: {e}"));
                for d in devices.iter_mut() {
                    for s in d.sensors.iter_mut() {
                        if s.desc.kind == SensorKind::Dynamic {
                            // 按各传感器自身的类型标错,不能统一写 F64
                            s.current = Reading::failed(Status::Error, s.desc.vtype);
                        }
                    }
                }
                return;
            }
        };
        let by_index: HashMap<u32, &IfRow> = rows.iter().map(|r| (r.index, r)).collect();

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
                    if s.desc.kind == SensorKind::Dynamic {
                        s.current = Reading::failed(Status::Error, s.desc.vtype);
                    }
                }
                continue;
            };

            let now = Instant::now();
            let wifi_tx_rate = if dev.sensors.iter().any(|s| s.desc.code.starts_with("wifi_")) {
                // BSD 名跨启动稳定(meta.name 就是它),CoreWLAN 本身按名寻址
                refresh_wifi(wlan.as_ref(), &dev.meta.name.clone(), dev)
            } else {
                None
            };

            let sample = Sample {
                rx_bytes: row.ibytes,
                tx_bytes: row.obytes,
                at: now,
            };
            let (rx_kbps, tx_kbps) = {
                let old = prev.insert(idx, sample);
                match old {
                    Some(o) if o.rx_bytes <= row.ibytes => {
                        let dt = now.duration_since(o.at).as_secs_f64();
                        if dt > 0.05 {
                            (
                                (row.ibytes - o.rx_bytes) as f64 / dt / 1024.0,
                                (row.obytes - o.tx_bytes) as f64 / dt / 1024.0,
                            )
                        } else {
                            (-1.0, -1.0) // 间隔过短,本轮跳过
                        }
                    }
                    _ => (-1.0, -1.0), // 首轮等待 / 计数器回绕,不给假速率
                }
            };
            if rx_kbps >= 0.0 {
                dev.set_reading(
                    "rx_kbps",
                    Reading::now(Value::F64((rx_kbps * 10.0).round() / 10.0)),
                );
                dev.set_reading(
                    "tx_kbps",
                    Reading::now(Value::F64((tx_kbps * 10.0).round() / 10.0)),
                );
            }
            dev.set_reading("link_up", Reading::now(Value::Bool(row.link_up())));
            // WiFi 口的链路速率优先取 CoreWLAN 协商速率:ifi_baudrate 在 WiFi 口上
            // 是栈给的标称值(实测 1.536Gbps vs 实际协商 864Mbps),报出去是假数据;
            // 以太网等有线口的 ifi_baudrate 才是真实协商速率,直接用。
            let link_mbps = wifi_tx_rate
                .filter(|v| *v > 0.0)
                .unwrap_or(row.baudrate as f64 / 1_000_000.0);
            dev.set_reading("link_speed_mbps", Reading::now(Value::F64(link_mbps)));
            dev.set_reading(
                "rx_total_mb",
                Reading::now(Value::F64(row.ibytes as f64 / 1024.0 / 1024.0)),
            );
            dev.set_reading(
                "tx_total_mb",
                Reading::now(Value::F64(row.obytes as f64 / 1024.0 / 1024.0)),
            );
            dev.set_reading("rx_errors", Reading::now(Value::U64(row.ierrors)));
            dev.set_reading("tx_errors", Reading::now(Value::U64(row.oerrors)));
        }
    }
}

// ---------------- WiFi 扩展 ----------------

/// WiFi 扩展传感器(仅 CoreWLAN 认得的站点接口声明)。清单与 Windows 版
/// `windows/net.rs` 的 `WIFI_SENSORS` 逐条一致(code/name/unit/vtype 全同),
/// 保证两个平台的 (category, device, sensor) 寻址一致。
///
/// macOS 上没有对应数据源的键**照样声明**,刷新时按状态标非 OK —— 键不消失,
/// 但也不填编出来的值。
struct WifiSensor {
    code: &'static str,
    name: &'static str,
    unit: &'static str,
    vtype: ValueType,
}

const WIFI_SENSORS: &[WifiSensor] = &[
    WifiSensor {
        code: "wifi_state",
        name: "WiFi State",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_ssid",
        name: "SSID",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_bssid",
        name: "BSSID",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_profile",
        name: "Profile",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_band",
        name: "Band",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_channel",
        name: "Channel",
        unit: "",
        vtype: ValueType::U64,
    },
    WifiSensor {
        code: "wifi_freq_mhz",
        name: "Frequency",
        unit: "MHz",
        vtype: ValueType::F64,
    },
    WifiSensor {
        code: "wifi_phy",
        name: "Radio Type",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_signal_pct",
        name: "Signal",
        unit: "%",
        vtype: ValueType::F64,
    },
    WifiSensor {
        code: "wifi_rssi_dbm",
        name: "RSSI",
        unit: "dBm",
        vtype: ValueType::F64,
    },
    WifiSensor {
        code: "wifi_rx_mbps",
        name: "Receive Rate",
        unit: "Mbps",
        vtype: ValueType::F64,
    },
    WifiSensor {
        code: "wifi_tx_mbps",
        name: "Transmit Rate",
        unit: "Mbps",
        vtype: ValueType::F64,
    },
    WifiSensor {
        code: "wifi_auth",
        name: "Authentication",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_cipher",
        name: "Cipher",
        unit: "",
        vtype: ValueType::Str,
    },
    WifiSensor {
        code: "wifi_radio_on",
        name: "Radio On",
        unit: "",
        vtype: ValueType::Bool,
    },
];

/// 刷新 WiFi 扩展键。
///
/// 状态语义(照 Windows 版):接口未关联时关联类字段本就无值,标 NotSupported;
/// 只有查询本身失败才标 Error —— 否则"没连 WiFi"会看起来像传感器故障。
///
/// macOS 特有的一档:已关联却读不到 SSID/BSSID,是定位服务(Location Services)
/// 没授权(见 `wlan.rs` 头注释),这两个键单独标 NoPermission,不与"未关联"
/// 混为一谈。其余 macOS 无数据源的键(profile / signal_pct / rx_mbps / cipher)
/// 保留 NotSupported。
fn refresh_wifi(wlan: Option<&Wlan>, name: &str, dev: &mut DeviceData) -> Option<f64> {
    let Some(d) = wlan.and_then(|w| w.query(name)) else {
        // 查询失败(接口刚消失 / WiFi 栈异常):整组含状态一律 Error
        mark_wifi(dev, Status::Error);
        return None;
    };

    // 先全标"未关联不提供",再逐项覆盖;wifi_state / wifi_radio_on 总有值
    mark_wifi(dev, Status::NotSupported);
    dev.set_reading("wifi_state", Reading::now(Value::Str(d.state.to_string())));
    set_bool(dev, "wifi_radio_on", d.radio_on);
    if !d.connected {
        return None;
    }

    // 频率要在搬走字段之前算(它借的是整个 d)
    let freq = d.freq_mhz();
    for (code, v) in [("wifi_ssid", &d.ssid), ("wifi_bssid", &d.bssid)] {
        match v {
            Some(s) if !s.is_empty() => {
                dev.set_reading(code, Reading::now(Value::Str(s.clone())));
            }
            // 已关联却读不到 = 定位服务未授权,如实报"无权限"
            _ => dev.set_reading(code, Reading::failed(Status::NoPermission, ValueType::Str)),
        }
    }
    set_str(dev, "wifi_band", d.band.unwrap_or_default().to_string());
    set_str(dev, "wifi_phy", d.phy.unwrap_or_default().to_string());
    set_str(dev, "wifi_auth", d.security.unwrap_or_default().to_string());
    set_u64(dev, "wifi_channel", d.channel.map(|c| c as u64));
    set_f64(dev, "wifi_freq_mhz", freq);
    set_f64(dev, "wifi_rssi_dbm", d.rssi_dbm.map(|v| v as f64));
    set_f64(dev, "wifi_tx_mbps", d.tx_mbps);
    // 返回协商速率,供调用方覆盖 WiFi 口的 link_speed_mbps(见调用处注释)
    d.tx_mbps
}

/// 把 wifi_* 动态键标为给定状态(含 wifi_state,调用方在需要时覆盖它)。
fn mark_wifi(dev: &mut DeviceData, status: Status) {
    for s in dev.sensors.iter_mut() {
        if s.desc.code.starts_with("wifi_") && s.desc.kind == SensorKind::Dynamic {
            s.current = Reading::failed(status, s.desc.vtype);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_msghdr2_layout_matches_sdk() {
        // net/if.h + net/if_var.h 的字段顺序落到实际偏移上,不靠肉眼。
        assert_eq!(std::mem::offset_of!(IfMsghdr2, ifm_addrs), 4);
        assert_eq!(std::mem::offset_of!(IfMsghdr2, ifm_flags), 8);
        assert_eq!(std::mem::offset_of!(IfMsghdr2, ifm_index), 12);
        assert_eq!(std::mem::offset_of!(IfMsghdr2, ifm_data), 32);
        assert_eq!(std::mem::offset_of!(IfData64, ifi_mtu), 8);
        assert_eq!(std::mem::offset_of!(IfData64, ifi_baudrate), 16);
        assert_eq!(std::mem::offset_of!(IfData64, ifi_ipackets), 24);
        assert_eq!(std::mem::offset_of!(IfData64, ifi_ibytes), 64);
        assert_eq!(std::mem::offset_of!(IfData64, ifi_obytes), 72);
    }

    #[test]
    fn sockaddr_dl_data_offset() {
        // net/if_dl.h:名字紧跟在 sdl_len/family/index/type/nlen/alen/slen 之后。
        assert_eq!(std::mem::offset_of!(libc::sockaddr_dl, sdl_data), 8);
    }

    #[test]
    fn physical_filter() {
        assert!(is_physical("en0", IFT_ETHER));
        assert!(is_physical("en12", IFT_ETHER));
        assert!(is_physical("en0", IFT_IEEE80211));
        // 虚拟/平台内部接口一律不要
        assert!(!is_physical("lo0", IFT_ETHER));
        assert!(!is_physical("utun3", IFT_ETHER));
        assert!(!is_physical("awdl0", IFT_ETHER));
        assert!(!is_physical("llw0", IFT_ETHER));
        assert!(!is_physical("ap1", IFT_ETHER));
        assert!(!is_physical("nan0", IFT_ETHER));
        assert!(!is_physical("bridge0", IFT_ETHER));
        assert!(!is_physical("vmenet0", IFT_ETHER));
        assert!(!is_physical("anpi0", IFT_ETHER));
        assert!(!is_physical("en", IFT_ETHER));
        assert!(!is_physical("enx", IFT_ETHER));
        // 类型不符(如 IFT_LOOP 的 0x18)也不收
        assert!(!is_physical("en0", 0x18));
    }

    #[test]
    fn link_state_needs_both_flags() {
        let row = |flags: u32| IfRow {
            index: 1,
            flags,
            mtu: 1500,
            baudrate: 0,
            ibytes: 0,
            obytes: 0,
            ipackets: 0,
            opackets: 0,
            ierrors: 0,
            oerrors: 0,
        };
        assert!(row(IFF_UP | IFF_RUNNING).link_up());
        assert!(!row(IFF_UP).link_up());
        assert!(!row(IFF_RUNNING).link_up());
        assert!(!row(0).link_up());
    }
}
