//! Disk provider(macOS):IOBlockStorageDriver 统计差分 + NVMe/ATA SMART + SMC NAND 补充。
//!
//! 语义与 Windows 版(`platform/windows/disk.rs`)逐 code 对齐,数据源换为 IOKit:
//! - 枚举:`IOBlockStorageDevice`,每块物理盘一个节点;**无介质的读卡器不算盘**
//!   (配不到整盘 `IOMedia` 子节点就跳过,与 Windows"每块物理盘"口径一致);
//! - 识别:`"Device Characteristics"`(Vendor/Product/Revision/Serial/Medium Type)、
//!   `"Protocol Characteristics"`(Physical Interconnect / Location)、
//!   整盘 `IOMedia`(`Whole`=`Yes`)的 `Size` / `BSD Name`;
//! - 动态吞吐/活动率:`IOBlockStorageDriver` `"Statistics"` 的
//!   `Bytes (Read/Write)` 与 `Total Time (Read/Write)`(**纳秒**)差分 ÷ 间隔,
//!   首轮只建基线不产值(保持 Waiting);
//! - 温度/磨损/写入量:NVMe 走 NVMeSMARTLib 插件(`NVMe SMART Capable`),
//!   SATA/ATA 走 ATA SMARTLib(`SMART Capable`),两路都无 → 按状态标不支持;
//! - SMC `TH0x`/`TH0a`/`TH0b`(NAND 温度)仅在**内置单盘**机器上作为该盘温度的
//!   补充来源,SMART 优先,不冒充。
//!
//! # 两个现实的坑
//!
//! 1. **APFS 同名不同义**:APFS 层(`AppleAPFSMedia`/`AppleAPFSVolume`)也有
//!    `"Statistics"` 字典,但键是 `"Bytes read from block device"` 等,完全不相干。
//!    这里按注册表父子关系只认 `IOBlockStorageDevice` 的 `IOBlockStorageDriver`
//!    子节点,并要求字典含 `"Bytes (Read)"` 键,读不错对象。
//! 2. **COM vtable 不能按槽位号数**:`IUNKNOWN_C_GUTS`(CFPlugInCOM.h)含
//!    `_reserved` 首字段,接口结构里又夹着 `version/revision` 两个数据字段,
//!    函数指针下标极易数错。这里一律按 `#[repr(C)]` 具名字段取函数指针,
//!    布局逐字段对齐 NVMeSMARTLibExternal.h(via smartmontools os_darwin.h)/
//!    ATASMARTLib.h / IOCFPlugIn.h / CFPlugInCOM.h 原文。
//!
//! 权限:macOS 12+ / Apple Silicon 普通用户可读 NVMe SMART(本机 macOS 27 实测);
//! 旧系统/部分驱动会以 `kIOReturnNotPrivileged`(0xe00002c1)拒绝,
//! 该盘温度/磨损标 [`Status::NoPermission`](crate::model::Status::NoPermission),不猜值。

use super::iokit::{self, CfDict, IoObject};
use super::smc;
use crate::model::{
    Category, DeviceBuilder, DeviceData, Reading, SensorKind, Status, Value, ValueType,
};
use crate::traits::Provider;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use core_foundation_sys::uuid::{CFUUIDBytes, CFUUIDCreateFromUUIDBytes, CFUUIDRef};
use io_kit_sys::types::{io_object_t, io_registry_entry_t, io_service_t};
use io_kit_sys::{
    keys::kIOServicePlane, IOObjectRelease, IORegistryEntryGetParentEntry,
    IORegistryEntryGetPath, IORegistryEntryGetRegistryEntryID,
};
use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------- CFPlugIn COM 基础设施 ----------------

/// COM 接口 vtable 的公共头(`IUNKNOWN_C_GUTS`,CFPlugInCOM.h):
/// `_reserved` 是 Microsoft COM 二进制标准的一部分,数槽位时最容易被漏掉的就是它。
#[repr(C)]
#[allow(dead_code)] // 按头文件完整映射;未调用的槽位仅占位
struct VtableHead {
    _reserved: *mut c_void,
    query_interface: Option<unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32>,
    add_ref: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    release: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
}

/// `IONVMeSMARTInterface` vtable(NVMeSMARTLibExternal.h,经 smartmontools
/// os_darwin.h 核对):头之后是 `version`/`revision` **数据字段**(不是函数指针,
/// 中间还有 4 字节对齐填充),再往后才是 SMARTReadData。
#[repr(C)]
#[allow(dead_code)] // get_identify_data 为布局占位,本 provider 不调用
struct NvmeSmartVtable {
    head: VtableHead,
    version: u16,
    revision: u16,
    smart_read_data: Option<unsafe extern "C" fn(*mut c_void, *mut u8) -> i32>,
    get_identify_data: Option<unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> i32>,
}

/// `IOATASMARTInterface` vtable(ATASMARTLib.h):SMARTReadData 之前有 4 个
/// 强制/可选函数槽位,必须原样占位才能落在正确偏移上。
#[repr(C)]
#[allow(dead_code)] // 仅 smart_read_data 被调用,其余按头文件占位
struct AtaSmartVtable {
    head: VtableHead,
    version: u16,
    revision: u16,
    enable_disable_operations: Option<unsafe extern "C" fn(*mut c_void, u8) -> i32>,
    enable_disable_autosave: Option<unsafe extern "C" fn(*mut c_void, u8) -> i32>,
    return_status: Option<unsafe extern "C" fn(*mut c_void, *mut u8) -> i32>,
    execute_offline: Option<unsafe extern "C" fn(*mut c_void, u8) -> i32>,
    smart_read_data: Option<unsafe extern "C" fn(*mut c_void, *mut u8) -> i32>,
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    /// IOCFPlugIn.h 的 `IOCreatePlugInInterfaceForService` / `IODestroyPlugInInterface`
    /// (io-kit-sys 未绑定,在此声明;CoreFoundation 的 UUID 函数走 core-foundation-sys)。
    fn IOCreatePlugInInterfaceForService(
        service: io_service_t,
        plugin_type: CFUUIDRef,
        interface_type: CFUUIDRef,
        the_interface: *mut *mut *mut VtableHead,
        the_score: *mut i32,
    ) -> i32;

    fn IODestroyPlugInInterface(interface: *mut *mut VtableHead) -> i32;
}

/// kIOCFPlugInInterfaceID = C244E858-109C-11D4-91D4-0050E4C6426F
/// (IOCFPlugIn.h 注释里的 UUID 逐字节展开)。
const UUID_PLUG_IN_INTERFACE: [u8; 16] = [
    0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F,
];
/// kIONVMeSMARTUserClientTypeID = AA0FA6F9-C2D6-457F-B10B-59A13253292F。
const UUID_NVME_SMART_USERCLIENT: [u8; 16] = [
    0xAA, 0x0F, 0xA6, 0xF9, 0xC2, 0xD6, 0x45, 0x7F, 0xB1, 0x0B, 0x59, 0xA1, 0x32, 0x53, 0x29, 0x2F,
];
/// kIONVMeSMARTInterfaceID = CCD1DB19-FD9A-4DAF-BF95-12454B230AB6。
const UUID_NVME_SMART_INTERFACE: [u8; 16] = [
    0xCC, 0xD1, 0xDB, 0x19, 0xFD, 0x9A, 0x4D, 0xAF, 0xBF, 0x95, 0x12, 0x45, 0x4B, 0x23, 0x0A, 0xB6,
];
/// kIOATASMARTUserClientTypeID = 24514B7A-2804-11D6-8A02-003065704866。
const UUID_ATA_SMART_USERCLIENT: [u8; 16] = [
    0x24, 0x51, 0x4B, 0x7A, 0x28, 0x04, 0x11, 0xD6, 0x8A, 0x02, 0x00, 0x30, 0x65, 0x70, 0x48, 0x66,
];
/// kIOATASMARTInterfaceID = 08ABE21C-20D4-11D6-8DF6-0003935A76B2。
const UUID_ATA_SMART_INTERFACE: [u8; 16] = [
    0x08, 0xAB, 0xE2, 0x1C, 0x20, 0xD4, 0x11, 0xD6, 0x8D, 0xF6, 0x00, 0x03, 0x93, 0x5A, 0x76, 0xB2,
];

/// `kIOReturnNotPrivileged`(0xe00002c1):旧系统/部分驱动拒绝非 root 读 SMART。
const K_IORETURN_NOT_PRIVILEGED: u32 = 0xe000_02c1;

/// 512B、8 字节对齐缓冲(COM 接口按结构体整体写入)。
#[repr(C, align(8))]
struct Aligned512([u8; 512]);

/// CFUUID(Create 语义),Drop 时 `CFRelease`。
struct CfUuid(CFUUIDRef);

impl CfUuid {
    fn from_bytes(b: &[u8; 16]) -> Option<CfUuid> {
        let bytes = uuid_bytes(b);
        // SAFETY: 默认 allocator(null);bytes 为值类型,无悬垂指针。
        let p = unsafe { CFUUIDCreateFromUUIDBytes(std::ptr::null(), bytes) };
        if p.is_null() {
            None
        } else {
            Some(CfUuid(p))
        }
    }
}

impl Drop for CfUuid {
    fn drop(&mut self) {
        // SAFETY: self.0 来自 *Create* 语义,恰好释放一次。
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

/// 插件对象句柄(`IOCFPlugInInterface **`)。销毁序列照 smartmontools os_darwin.cpp:
/// 先 Release 各接口,再 `IODestroyPlugInInterface`(本 guard 只做后者,
/// 接口的 Release 由 [`InterfaceGuard`] 负责,drop 顺序天然先接口后插件)。
struct PluginGuard(*mut *mut VtableHead);

impl Drop for PluginGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: self.0 由 IOCreatePlugInInterfaceForService 返回,只销毁一次。
            unsafe { IODestroyPlugInInterface(self.0) };
        }
    }
}

/// COM 接口对象句柄(`T **`),Drop 时调 vtable 的 `Release` 平掉 QueryInterface
/// 带来的引用计数。NVMe/ATA 两种接口的 vtable 头布局一致,统一用
/// [`VtableHead`] 视图管理生命周期。
struct InterfaceGuard {
    handle: *mut *mut VtableHead,
}

impl Drop for InterfaceGuard {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // SAFETY: handle 由 QueryInterface 返回(引用计数 +1);句柄首字是
        // vtable 指针,VtableHead 是其布局前缀。
        unsafe {
            if let Some(release) = (**self.handle).release {
                release(self.handle as *mut c_void);
            }
        }
    }
}

/// CFUUID 字节形式(REFIID 按值传递)。
fn uuid_bytes(b: &[u8; 16]) -> CFUUIDBytes {
    CFUUIDBytes {
        byte0: b[0],
        byte1: b[1],
        byte2: b[2],
        byte3: b[3],
        byte4: b[4],
        byte5: b[5],
        byte6: b[6],
        byte7: b[7],
        byte8: b[8],
        byte9: b[9],
        byte10: b[10],
        byte11: b[11],
        byte12: b[12],
        byte13: b[13],
        byte14: b[14],
        byte15: b[15],
    }
}

/// IOReturn / HRESULT → [`Status`]。只有"权限不足"有确定语义,
/// 其余失败不猜原因,一律 `Error`。
fn map_iokit_error(rc: i32) -> Status {
    if rc as u32 == K_IORETURN_NOT_PRIVILEGED {
        Status::NoPermission
    } else {
        Status::Error
    }
}

/// IOCFPlugIn 调用序列(smartmontools os_darwin.cpp 同式):
/// `IOCreatePlugInInterfaceForService`(user client 类型 UUID)→
/// `QueryInterface`(接口 UUID)→ 在 `call` 里用具体接口 → 逆序销毁。
fn open_plugin<T>(
    service: io_service_t,
    user_client_uuid: &[u8; 16],
    interface_uuid: &[u8; 16],
    call: impl FnOnce(&InterfaceGuard) -> Result<T, Status>,
) -> Result<T, Status> {
    let plugin_type = CfUuid::from_bytes(&UUID_PLUG_IN_INTERFACE).ok_or(Status::Error)?;
    let client_type = CfUuid::from_bytes(user_client_uuid).ok_or(Status::Error)?;

    let mut plugin: *mut *mut VtableHead = std::ptr::null_mut();
    let mut score: i32 = 0;
    // SAFETY: service 为有效 io_service_t;两个 CFUUID 有效;出参按 Create 语义
    // 返回插件对象,失败时保持 null(随后判空)。
    // 参数顺序:pluginType = user client 类型 UUID(要加载哪个插件),
    // interfaceType = kIOCFPlugInInterfaceID(向插件要哪个接口)——
    // 与 smartmontools os_darwin.cpp 的调用逐参一致。
    let rc = unsafe {
        IOCreatePlugInInterfaceForService(
            service,
            client_type.0,
            plugin_type.0,
            &mut plugin,
            &mut score,
        )
    };
    if rc != 0 || plugin.is_null() {
        return Err(map_iokit_error(rc));
    }
    let plugin_guard = PluginGuard(plugin);

    // SAFETY: plugin 句柄首字是 vtable 指针;query_interface 槽位按 CFPlugInCOM.h
    // 布局(带 _reserved 头)必在。
    let query_interface = match unsafe { (**plugin_guard.0).query_interface } {
        Some(f) => f,
        None => return Err(Status::Error),
    };
    let mut iface: *mut c_void = std::ptr::null_mut();
    // SAFETY: plugin_guard.0 是有效插件对象(即方法的 self);REFIID 按值传
    // 16 字节 CFUUIDBytes;出参收接口对象指针。
    let hres = unsafe {
        query_interface(
            plugin_guard.0 as *mut c_void,
            uuid_bytes(interface_uuid),
            &mut iface,
        )
    };
    if hres < 0 || iface.is_null() {
        return Err(map_iokit_error(hres));
    }
    let guard = InterfaceGuard {
        handle: iface as *mut *mut VtableHead,
    };
    call(&guard)
}

// ---------------- NVMe SMART ----------------

/// NVMe SMART/Health Log Page(02h,512B)解析值。
/// 字段偏移/语义对齐 NVMe spec(与 Windows 版 `nvme_health` 同源)。
#[derive(Debug, Clone, Copy)]
pub struct NvmeSmartLog {
    /// critical_warning(u8 @0,位标志)。
    pub critical_warning: u8,
    /// 温度(u16 LE @1,开尔文 → °C = K − 273.15;报 0 视为未给,取 NaN)。
    pub temperature_c: f64,
    /// 可用备件 %(u8 @3)。
    pub spare_pct: f64,
    /// 寿命已用 %(u8 @5)。
    pub used_pct: f64,
    /// 累计写入(u128 LE @48 的低 64 位,1 单位 = 512000 字节)。
    pub data_units_written: u64,
    /// 写入总量 TB(单位数 × 512000 ÷ 1024⁴,与 Windows 版同式)。
    pub written_tb: f64,
    /// 上电次数 / 累计通电小时(u128 LE 的低 64 位)。
    pub power_cycles: u64,
    pub power_on_hours: u64,
}

fn le16(buf: &[u8; 512], off: usize) -> f64 {
    u16::from_le_bytes([buf[off], buf[off + 1]]) as f64
}

fn le64(buf: &[u8; 512], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

fn parse_nvme_smart_log(buf: &[u8; 512]) -> NvmeSmartLog {
    let temp_k = le16(buf, 1);
    let units = le64(buf, 48); // u128 LE @48 的低 64 位
    NvmeSmartLog {
        critical_warning: buf[0],
        temperature_c: if temp_k > 0.0 {
            temp_k - 273.15
        } else {
            f64::NAN
        },
        spare_pct: buf[3] as f64,
        used_pct: buf[5] as f64,
        data_units_written: units,
        written_tb: units as f64 * 512_000.0 / 1024.0 / 1024.0 / 1024.0 / 1024.0,
        power_cycles: le64(buf, 112),
        power_on_hours: le64(buf, 128),
    }
}

/// 读 NVMe SMART:`IOCreatePlugInInterfaceForService` → `QueryInterface`
/// (kIONVMeSMARTInterfaceID)→ `SMARTReadData`(512B nvme_smart_log)。
fn read_nvme_smart(service: io_service_t) -> Result<NvmeSmartLog, Status> {
    open_plugin(
        service,
        &UUID_NVME_SMART_USERCLIENT,
        &UUID_NVME_SMART_INTERFACE,
        |guard| {
            // SAFETY: guard.handle 是 QueryInterface 返回的接口对象;其首字是
            // vtable 指针,按 NvmeSmartVtable 布局读取函数槽位。
            let vt = unsafe { &**(guard.handle as *mut *mut NvmeSmartVtable) };
            let smart_read_data = vt.smart_read_data.ok_or(Status::Error)?;
            let mut buf = Aligned512([0u8; 512]);
            // SAFETY: self 即接口句柄;buf 为 512B 对齐缓冲,满足 SMARTReadData
            // 写入 nvme_smart_log 的约定。
            let rc = unsafe { smart_read_data(guard.handle as *mut c_void, buf.0.as_mut_ptr()) };
            if rc != 0 {
                return Err(map_iokit_error(rc));
            }
            Ok(parse_nvme_smart_log(&buf.0))
        },
    )
}

// ---------------- ATA SMART(SATA 盘温度) ----------------

/// SATA SMART 属性区(512B READ DATA)→ 温度:属性 194 优先、190 兜底,
/// 候选取值与 Windows 版 `parse_smart_temp` 同式(raw 低 32 位 → raw[1] → raw[6]
/// → 归一化 value,全部要求 1..=120 的合理范围)。属性表自偏移 2 起、12B/条。
fn parse_ata_smart_temp(data: &[u8]) -> Option<f64> {
    if data.len() < 362 {
        return None;
    }
    let attr_temp = |off: usize| -> Option<u32> {
        let id = data[off];
        if id != 194 && id != 190 {
            return None;
        }
        let raw = &data[off + 4..off + 12];
        let le32 = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let value = data[off + 3] as u32;
        [le32, raw[1] as u32, raw[6] as u32, value]
            .into_iter()
            .find(|t| (1..=120).contains(t))
    };
    let mut fallback = None;
    for off in (2..362).step_by(12) {
        if data[off] == 194 {
            if let Some(t) = attr_temp(off) {
                return Some(t as f64);
            }
        } else if data[off] == 190 && fallback.is_none() {
            fallback = attr_temp(off);
        }
    }
    fallback.map(|t| t as f64)
}

/// 读 ATA SMART 温度(ATASMARTLib 插件,`SMART Capable` 盘):
/// 与 NVMe 同一调用序列,接口换 kIOATASMARTInterfaceID,取 `SMARTReadData`。
fn read_ata_smart_temp(service: io_service_t) -> Result<f64, Status> {
    open_plugin(
        service,
        &UUID_ATA_SMART_USERCLIENT,
        &UUID_ATA_SMART_INTERFACE,
        |guard| {
            // SAFETY: 同 read_nvme_smart;按 AtaSmartVtable 布局取函数槽位。
            let vt = unsafe { &**(guard.handle as *mut *mut AtaSmartVtable) };
            let smart_read_data = vt.smart_read_data.ok_or(Status::Error)?;
            let mut buf = Aligned512([0u8; 512]);
            // SAFETY: SMARTReadData(void*, ATASMARTData*) 写满 512B,buf 满足约定。
            let rc = unsafe { smart_read_data(guard.handle as *mut c_void, buf.0.as_mut_ptr()) };
            if rc != 0 {
                return Err(map_iokit_error(rc));
            }
            parse_ata_smart_temp(&buf.0).ok_or(Status::NotSupported)
        },
    )
}

/// 一次 SMART 读取的结果:温度(来源见 `source`)+ NVMe 专属字段。
struct SmartRead {
    temperature_c: Option<f64>,
    /// 温度来源:`"nvme"` / `"ata"`。
    #[allow(dead_code)] // 供诊断/探针区分来源
    source: &'static str,
    nvme: Option<NvmeSmartLog>,
}

/// 按能力标记选一条 SMART 通路:NVMe 优先,其次 ATA;都不支持 → `NotSupported`。
fn read_smart(
    nvme_capable: bool,
    ata_capable: bool,
    service: io_service_t,
) -> Result<SmartRead, Status> {
    if nvme_capable {
        let log = read_nvme_smart(service)?;
        return Ok(SmartRead {
            temperature_c: log.temperature_c.is_finite().then_some(log.temperature_c),
            source: "nvme",
            nvme: Some(log),
        });
    }
    if ata_capable {
        let t = read_ata_smart_temp(service)?;
        return Ok(SmartRead {
            temperature_c: Some(t),
            source: "ata",
            nvme: None,
        });
    }
    Err(Status::NotSupported)
}

// ---------------- SMC NAND 温度补充 ----------------

/// SMC NAND 温度键(社区键位;读不到就是读不到)。
const SMC_TEMP_KEYS: [&str; 3] = ["TH0x", "TH0a", "TH0b"];
/// 温度合理性区间(°C,与 GPU provider 同式)。
const TEMP_PLAUSIBLE: std::ops::RangeInclusive<f64> = 10.0..=115.0;
/// 良值保持窗口:SMC 偶发读失败时不把 Ok 打回 Error(与 GPU provider 同式)。
const TEMP_HOLD: Duration = Duration::from_secs(15);

/// SMC NAND 温度补充:多键取最大,合理性过滤 + 15s 良值保持。
/// **归属规则**:只在内置单盘机器上作为该盘温度的补充来源,且只在 SMART
/// 给不出温度时使用 —— 不冒充 SMART 温度。
fn smc_nand_temp(hold: &mut Option<(f64, Instant)>, now: Instant) -> Option<f64> {
    let mut max: Option<f64> = None;
    for key in SMC_TEMP_KEYS {
        if let Some(v) = smc::read_f64(key).filter(|v| TEMP_PLAUSIBLE.contains(v)) {
            max = Some(max.map_or(v, |m: f64| m.max(v)));
        }
    }
    if let Some(v) = max {
        *hold = Some((v, now));
        return Some(v);
    }
    hold.filter(|(_, at)| now.duration_since(*at) < TEMP_HOLD)
        .map(|(v, _)| v)
}

// ---------------- 枚举与识别 ----------------

/// `IOBlockStorageDriver` `"Statistics"` 字典原值(累计计数,时间单位**纳秒**)。
/// 单位对齐 IOBlockStorageDriver.h 原文("number of nanoseconds spent …")。
#[derive(Debug, Default, Clone, Copy)]
pub struct StatsSnapshot {
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub ops_read: u64,
    pub ops_write: u64,
    pub errors_read: u64,
    pub errors_write: u64,
    /// `Total Time (Read)`,纳秒。
    pub time_read_ns: u64,
    /// `Total Time (Write)`,纳秒。
    pub time_write_ns: u64,
}

/// 一个块存储设备的**原值**快照(识别 + Statistics + 两路 SMART),
/// 供探针在真机上核对单位与量级;字段保持来源原值,换算见类型注释。
#[derive(Debug)]
pub struct DiskSource {
    pub entry_id: Option<u64>,
    pub device_path: Option<String>,
    pub bsd_name: Option<String>,
    pub size_bytes: Option<u64>,
    pub vendor_name: Option<String>,
    pub product_name: Option<String>,
    pub revision: Option<String>,
    pub serial: Option<String>,
    pub medium_type: Option<String>,
    pub interconnect: Option<String>,
    pub interconnect_location: Option<String>,
    pub nvme_smart_capable: bool,
    pub ata_smart_capable: bool,
    pub stats: Option<StatsSnapshot>,
    pub nvme_smart: Result<NvmeSmartLog, Status>,
    pub ata_smart_temp: Result<f64, Status>,
}

/// 块存储设备枚举结果:识别信息 + 句柄(供 refresh 复用)。
struct DiskEntry {
    /// `IOBlockStorageDevice`:SMART user client 的入口。
    dev: IoObject,
    /// `IOBlockStorageDriver`(子节点):`"Statistics"` 所在节点。
    driver: Option<IoObject>,
    entry_id: u64,
    /// IORegistry 路径(`pnp_id` 的值;macOS 无 PNP 概念,这是同等的稳定设备身份串)。
    path: Option<String>,
    /// 整盘 BSD 名(`disk0`…)。
    bsd: Option<String>,
    size_bytes: Option<u64>,
    vendor_name: Option<String>,
    product_name: Option<String>,
    revision: Option<String>,
    serial: Option<String>,
    /// `"Medium Type"` 映射后的 `"SSD"` / `"HDD"`。
    medium: Option<String>,
    /// `"Physical Interconnect"`(如 `"Apple Fabric"` / `"SATA"` / `"USB"`)。
    bus: Option<String>,
    /// `"Physical Interconnect Location"`(`"Internal"` / `"External"`)。
    location: Option<String>,
    nvme_capable: bool,
    ata_capable: bool,
    /// SMC NAND 温度补充是否允许(仅内置单盘机器)。
    smc_fallback: bool,
}

/// 嵌套字典取字符串(如 `"Device Characteristics"` 下的 `"Product Name"`)。
fn nested_string(props: &CfDict, sub: &str, key: &str) -> Option<String> {
    iokit::nested_value(props, sub, key).and_then(iokit::cf_string)
}

/// 节点在 IOService 平面的父节点 registry ID(设备↔驱动↔介质 靠它配对,
/// 不猜枚举顺序)。
fn parent_entry_id(entry: io_object_t) -> Option<u64> {
    let mut parent: io_object_t = 0;
    // SAFETY: entry 有效;平面名为常量字符串;成功时 parent 是新引用,由本函数释放。
    let rc = unsafe {
        IORegistryEntryGetParentEntry(entry as io_registry_entry_t, kIOServicePlane, &mut parent)
    };
    if rc != 0 || parent == 0 {
        return None;
    }
    let mut id: u64 = 0;
    // SAFETY: parent 是刚取得的有效引用;id 为合法出参。
    let rc2 = unsafe { IORegistryEntryGetRegistryEntryID(parent as io_registry_entry_t, &mut id) };
    // SAFETY: parent 只释放一次。
    unsafe { IOObjectRelease(parent) };
    (rc2 == 0 && id != 0).then_some(id)
}

/// Registry 节点的 IOService 平面路径(如 `IOService:/…/NS_01@1`)。
fn device_path(entry: &IoObject) -> Option<String> {
    let mut buf = [0 as c_char; 512];
    // SAFETY: buf 512 字节;成功时内核写入 NUL 结尾路径。
    let rc = unsafe {
        IORegistryEntryGetPath(
            entry.raw() as io_registry_entry_t,
            kIOServicePlane,
            buf.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: buf 是刚才由内核写入的 NUL 结尾字符串。
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(cstr.to_string_lossy().into_owned())
}

/// 枚举全部带介质的物理盘:设备 → 驱动 → 整盘介质,按注册表父子关系配对。
fn enumerate() -> Vec<DiskEntry> {
    // 驱动候选:(父 ID, 自身 ID, 句柄)。APFS 层的 "Statistics" 键完全不同,
    // 以 "Bytes (Read)" 键 + IOBlockStorageDevice 父配对双重判据。
    let mut driver_pool: Vec<(u64, u64, IoObject)> = Vec::new();
    for d in iokit::matching_services("IOBlockStorageDriver") {
        let Some(props) = iokit::registry_props(&d) else {
            continue;
        };
        if iokit::nested_value(&props, "Statistics", "Bytes (Read)").is_none() {
            continue;
        }
        let (Some(pid), Some(eid)) = (parent_entry_id(d.raw()), iokit::entry_id(&d)) else {
            continue;
        };
        driver_pool.push((pid, eid, d));
    }

    // 整盘介质候选:(父 ID(=驱动), BSD 名, Size)。Whole=Yes 过滤掉分区;
    // APFS 合成盘(AppleAPFSMedia)父节点不是驱动,配不上,自然出局。
    let mut media_pool: Vec<(u64, Option<String>, Option<u64>)> = Vec::new();
    for m in iokit::matching_services("IOMedia") {
        let Some(props) = iokit::registry_props(&m) else {
            continue;
        };
        if iokit::dict_bool(&props, "Whole") != Some(true) {
            continue;
        }
        let Some(pid) = parent_entry_id(m.raw()) else {
            continue;
        };
        let size = iokit::dict_i64(&props, "Size")
            .filter(|v| *v > 0)
            .map(|v| v as u64);
        media_pool.push((pid, iokit::dict_string(&props, "BSD Name"), size));
    }

    let mut out = Vec::new();
    for dev in iokit::matching_services("IOBlockStorageDevice") {
        let Some(props) = iokit::registry_props(&dev) else {
            continue;
        };
        let Some(dev_eid) = iokit::entry_id(&dev) else {
            continue;
        };
        let d_idx = driver_pool
            .iter()
            .position(|(pid, _, _)| *pid == dev_eid);
        // 介质挂在驱动下;配不到整盘介质 = 无介质(空读卡器),不算一块盘
        let media = d_idx.and_then(|i| {
            let (_, driver_eid, _) = &driver_pool[i];
            media_pool
                .iter()
                .find(|(pid, _, _)| pid == driver_eid)
                .map(|(_, bsd, size)| (bsd.clone(), *size))
        });
        let Some((bsd, size_bytes)) = media else {
            continue;
        };
        let driver = d_idx.map(|i| driver_pool.remove(i).2);
        let path = device_path(&dev);

        let dev_chars = "Device Characteristics";
        let proto_chars = "Protocol Characteristics";
        let medium = nested_string(&props, dev_chars, "Medium Type").and_then(|m| match m.as_str() {
            "Solid State" => Some("SSD".to_string()),
            "Rotational" => Some("HDD".to_string()),
            _ => None,
        });
        out.push(DiskEntry {
            dev,
            driver,
            entry_id: dev_eid,
            path,
            bsd,
            size_bytes,
            vendor_name: nested_string(&props, dev_chars, "Vendor Name"),
            product_name: nested_string(&props, dev_chars, "Product Name"),
            revision: nested_string(&props, dev_chars, "Product Revision Level"),
            serial: nested_string(&props, dev_chars, "Serial Number"),
            medium,
            bus: nested_string(&props, proto_chars, "Physical Interconnect"),
            location: nested_string(&props, proto_chars, "Physical Interconnect Location"),
            nvme_capable: iokit::dict_bool(&props, "NVMe SMART Capable") == Some(true),
            ata_capable: iokit::dict_bool(&props, "SMART Capable") == Some(true),
            smc_fallback: false,
        });
    }

    // 归属规则:SMC NAND 温度(TH0x…)无法归属到具体盘,只在**内置单盘**机器上
    // 作为该盘温度的补充来源。虚拟盘(挂载的 DMG/VHD,Location="File")不是
    // 归属候选,不参与计数。
    let internal_only: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, e)| e.location.as_deref() == Some("Internal"))
        .map(|(i, _)| i)
        .collect();
    if internal_only.len() == 1 {
        out[internal_only[0]].smc_fallback = true;
    }
    out
}

/// 读 `IOBlockStorageDriver` 的 `"Statistics"` 原值。
/// `"Bytes (Read)"` 键是该字典的身份判据(APFS 同名不同义);
/// 其余计数键由 IOStorageFamily 全量填充,缺失按 0 记。
fn read_stats(driver: &IoObject) -> Option<StatsSnapshot> {
    let props = iokit::registry_props(driver)?;
    iokit::nested_value(&props, "Statistics", "Bytes (Read)")?;
    let n = |key: &str| -> u64 { iokit::nested_i64(&props, "Statistics", key).unwrap_or(0).max(0) as u64 };
    Some(StatsSnapshot {
        bytes_read: n("Bytes (Read)"),
        bytes_written: n("Bytes (Write)"),
        ops_read: n("Operations (Read)"),
        ops_write: n("Operations (Write)"),
        errors_read: n("Errors (Read)"),
        errors_write: n("Errors (Write)"),
        time_read_ns: n("Total Time (Read)"),
        time_write_ns: n("Total Time (Write)"),
    })
}

/// 源原值快照(探针与 provider 共用枚举路径)。
pub fn source_snapshots() -> Vec<DiskSource> {
    let entries = enumerate();
    entries
        .iter()
        .map(|e| DiskSource {
            entry_id: Some(e.entry_id),
            device_path: e.path.clone(),
            bsd_name: e.bsd.clone(),
            size_bytes: e.size_bytes,
            vendor_name: e.vendor_name.clone(),
            product_name: e.product_name.clone(),
            revision: e.revision.clone(),
            serial: e.serial.clone(),
            medium_type: e.medium.clone(),
            interconnect: e.bus.clone(),
            interconnect_location: e.location.clone(),
            nvme_smart_capable: e.nvme_capable,
            ata_smart_capable: e.ata_capable,
            stats: e.driver.as_ref().and_then(read_stats),
            nvme_smart: if e.nvme_capable {
                read_nvme_smart(e.dev.raw())
            } else {
                Err(Status::NotSupported)
            },
            ata_smart_temp: if e.ata_capable {
                read_ata_smart_temp(e.dev.raw())
            } else {
                Err(Status::NotSupported)
            },
        })
        .collect()
}

// ---------------- Provider ----------------

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 上一轮 Statistics 采样(差分算速率)。
#[derive(Clone, Copy)]
struct StatsSample {
    bytes_read: u64,
    bytes_written: u64,
    /// `Total Time (Read+Write)`,纳秒(累计忙碌时间)。
    active_ns: u64,
    at: Instant,
}

struct DiskState {
    dev: IoObject,
    driver: Option<IoObject>,
    nvme_capable: bool,
    ata_capable: bool,
    smc_fallback: bool,
    /// SMC 良值保持(SMART 温度的补充源)。
    smc_hold: Option<(f64, Instant)>,
    prev: Option<StatsSample>,
}

struct ProviderState {
    disks: Vec<DiskState>,
    temp_tick: u32,
}

pub struct DiskProvider {
    inner: Mutex<ProviderState>,
}

impl DiskProvider {
    pub fn new() -> DiskProvider {
        DiskProvider {
            inner: Mutex::new(ProviderState {
                disks: Vec::new(),
                temp_tick: 0,
            }),
        }
    }
}

impl Default for DiskProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for DiskProvider {
    fn describe(&self) -> &str {
        "mac-disk"
    }

    fn category(&self) -> Category {
        Category::Disk
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let entries = enumerate();
        if entries.is_empty() {
            return Err("未发现物理磁盘(无带介质的块存储设备)".into());
        }

        let mut devices = Vec::new();
        let mut states = Vec::new();
        for e in entries {
            // 静态识别(字段与 Windows 版一致:id/name/vendor/model/serial
            // + bus_type/media_type/pnp_id/capacity_gb)。macOS 无 PNP 概念,
            // `pnp_id` 填 IORegistry 路径作为同等的稳定设备身份串。
            let id = match &e.bsd {
                Some(bsd) => format!("disk.{bsd}"),
                None => format!("disk.e{:x}", e.entry_id),
            };
            let model = e
                .product_name
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| e.bsd.clone())
                .unwrap_or_else(|| id.clone());
            let vendor = e
                .vendor_name
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Storage".to_string()); // 与 Windows 版常量一致
            let mut b = DeviceBuilder::new(id, model.clone())
                .vendor(vendor)
                .model(model);
            if let Some(serial) = e.serial.clone().filter(|s| !s.is_empty()) {
                b = b.serial(serial.trim().to_string());
            }
            if let Some(bus) = &e.bus {
                b = b.static_str("bus_type", "Bus Type", bus.clone());
            }
            if let Some(medium) = &e.medium {
                b = b.static_str("media_type", "Media Type", medium.clone());
            }
            if let Some(path) = &e.path {
                b = b.static_str("pnp_id", "PNP Device ID", path.clone());
            }
            if let Some(size) = e.size_bytes {
                b = b.static_f64(
                    "capacity_gb",
                    "Capacity",
                    "GB",
                    crate::util::bytes_to_gb(size),
                );
            }

            // 温度/磨损/写入量:>0 才声明(与 Windows 同规则,0 值不冒充有效读数)
            let smart = read_smart(e.nvme_capable, e.ata_capable, e.dev.raw());
            let mut temp_now: Option<f64> = None;
            if let Ok(s) = &smart {
                temp_now = s.temperature_c;
                if let Some(nvme) = &s.nvme {
                    if nvme.used_pct > 0.0 {
                        b = b.dynamic_now(
                            "wear_pct",
                            "Percentage Used",
                            "%",
                            Value::F64(nvme.used_pct),
                        );
                    }
                    if nvme.written_tb > 0.0 {
                        b = b.dynamic_now(
                            "written_tb",
                            "Total Written",
                            "TB",
                            Value::F64(round2(nvme.written_tb)),
                        );
                    }
                    if nvme.spare_pct > 0.0 {
                        b = b.dynamic_now(
                            "spare_pct",
                            "Available Spare",
                            "%",
                            Value::F64(nvme.spare_pct),
                        );
                    }
                }
            }
            // SMC NAND 补充:SMART 给不出温度才用
            let mut smc_hold = None;
            temp_now = temp_now.or_else(|| {
                e.smc_fallback
                    .then(|| smc_nand_temp(&mut smc_hold, Instant::now()))
                    .flatten()
            });
            b = match temp_now {
                Some(t) => {
                    b.dynamic_now("temperature_c", "Temperature", "°C", Value::F64(round1(t)))
                }
                None => b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64),
            };
            b = b.dynamic("activity_pct", "Activity", "%", ValueType::F64)
                .dynamic("read_kbps", "Read Rate", "KB/s", ValueType::F64)
                .dynamic("write_kbps", "Write Rate", "KB/s", ValueType::F64);

            devices.push(b.build());
            states.push(DiskState {
                dev: e.dev,
                driver: e.driver,
                nvme_capable: e.nvme_capable,
                ata_capable: e.ata_capable,
                smc_fallback: e.smc_fallback,
                smc_hold,
                prev: None,
            });
        }

        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = ProviderState {
            disks: states,
            temp_tick: 0,
        };
        Ok(devices)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.temp_tick = inner.temp_tick.wrapping_add(1);
        let temp_tick = inner.temp_tick;
        let now = Instant::now();

        for (i, st) in inner.disks.iter_mut().enumerate() {
            let Some(dev) = devices.get_mut(i) else {
                break;
            };

            // 吞吐/活动率:Statistics 差分。首轮只建基线不产值(保持 Waiting)。
            if let Some(driver) = &st.driver {
                if let Some(s) = read_stats(driver) {
                    let sample = StatsSample {
                        bytes_read: s.bytes_read,
                        bytes_written: s.bytes_written,
                        active_ns: s.time_read_ns.saturating_add(s.time_write_ns),
                        at: now,
                    };
                    let prev = st.prev.replace(sample);
                    if let Some(prev) = prev {
                        let dt = now.duration_since(prev.at).as_secs_f64();
                        if dt >= 0.05 {
                            let d_read = sample.bytes_read.saturating_sub(prev.bytes_read);
                            let d_write = sample.bytes_written.saturating_sub(prev.bytes_written);
                            let d_active = sample.active_ns.saturating_sub(prev.active_ns);
                            dev.set_reading(
                                "read_kbps",
                                Reading::now(Value::F64(round1(d_read as f64 / dt / 1024.0))),
                            );
                            dev.set_reading(
                                "write_kbps",
                                Reading::now(Value::F64(round1(d_write as f64 / dt / 1024.0))),
                            );
                            let pct = (d_active as f64 / dt / 1e9 * 100.0).clamp(0.0, 100.0);
                            dev.set_reading("activity_pct", Reading::now(Value::F64(round1(pct))));
                        }
                    }
                }
            }

            // 温度/磨损/写入量:终态(不支持/无权限)不再重试;SATA 通路拿到值后
            // 每 16 轮才复查(与 Windows 同式)。
            let temp_sensor = dev
                .sensors
                .iter()
                .find(|s| s.desc.code == "temperature_c")
                .map(|s| (s.desc.kind, s.current.status));
            let Some((kind, status)) = temp_sensor else {
                continue;
            };
            if kind != SensorKind::Dynamic {
                continue;
            }
            match status {
                Status::NotSupported | Status::NoPermission => continue,
                Status::Ok if temp_tick % 16 != 0 => {
                    if !st.nvme_capable {
                        continue;
                    }
                }
                _ => {}
            }

            let smart = read_smart(st.nvme_capable, st.ata_capable, st.dev.raw());
            if let Ok(s) = &smart {
                if let Some(nvme) = &s.nvme {
                    // 磨损/备件/累计写入:变化慢但必须持续刷新,否则冻结在 probe 值
                    if nvme.used_pct > 0.0 {
                        dev.set_reading("wear_pct", Reading::now(Value::F64(nvme.used_pct)));
                    }
                    if nvme.written_tb > 0.0 {
                        dev.set_reading(
                            "written_tb",
                            Reading::now(Value::F64(round2(nvme.written_tb))),
                        );
                    }
                    if nvme.spare_pct > 0.0 {
                        dev.set_reading("spare_pct", Reading::now(Value::F64(nvme.spare_pct)));
                    }
                }
            }
            // 温度:SMART 优先,SMC NAND 只做补充
            let temp = smart
                .as_ref()
                .ok()
                .and_then(|s| s.temperature_c)
                .or_else(|| {
                    st.smc_fallback
                        .then(|| smc_nand_temp(&mut st.smc_hold, now))
                        .flatten()
                });
            match temp {
                Some(t) => {
                    dev.set_reading("temperature_c", Reading::now(Value::F64(round1(t))))
                }
                None => {
                    // 读到了 SMART 但没温度数据 → 不支持;读取失败 → 原状态
                    // (无权限/错误),不猜值
                    let status = match &smart {
                        Err(s) => *s,
                        Ok(_) => Status::NotSupported,
                    };
                    dev.set_reading("temperature_c", Reading::failed(status, ValueType::F64));
                }
            }
        }
    }
}
