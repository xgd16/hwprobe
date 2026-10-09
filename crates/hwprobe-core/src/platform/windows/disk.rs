//! 磁盘 provider:物理盘识别 + 温度/磨损 + 性能计数。
//!
//! 数据链(降级顺序):
//! 1. 设备枚举:WMI Win32_DiskDrive(Index/型号/序列号/PNPID)
//! 2. 总线类型/寻道惩罚(SSD 判定):IOCTL_STORAGE_QUERY_PROPERTY
//! 3. NVMe 健康日志:IOCTL 协议专属查询(温度/磨损/写入量/备件)
//! 4. SATA SMART 温度:两条路径均需管理员——
//!    a) WMI MSStorageDriver_FailurePredictData 属性 194/190(Intel RST 等
//!       虚拟存储栈上唯一可用;非管理员查询返回空,属正常现象);
//!    b) ATA PASS-THROUGH(12) DATA-IN 读属性区(标准 AHCI 栈可用,
//!       RST 会以 ERROR_INVALID_FUNCTION/ERROR_REVISION_MISMATCH 拒绝)
//! 5. 性能:IOCTL_DISK_PERFORMANCE 差分(不依赖 PDH,规避性能计数器
//!    被安全软件干扰的机器)

#![cfg(target_os = "windows")]

use crate::model::{
    Category, DeviceBuilder, DeviceData, Reading, SensorKind, Status, Value, ValueType,
};
use crate::platform::windows::shared_wmi;
use crate::traits::Provider;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::IO::DeviceIoControl;

crate::wmi_struct!(WmiDiskDrive, "Win32_DiskDrive", {
    Index: u32,
    Model: String,
    SerialNumber: String,
    Size: u64,
    PNPDeviceID: String,
    MediaType: String,
    InterfaceType: String,
});

crate::wmi_struct!(WmiSmartData, "MSStorageDriver_FailurePredictData", {
    InstanceName: String,
    VendorSpecific: Vec<u8>,
});

// ---- IOCTL 结构(自建,避免依赖 windows crate 中分散的存储类型) ----

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional: [u8; 64], // 足够容纳 protocol specific data
}

#[repr(C)]
#[derive(Default)]
struct StorageDeviceDescriptorHeader {
    version: u32,
    size: u32,
}

#[repr(C)]
#[derive(Default)]
struct DeviceSeekPenaltyDescriptor {
    version: u32,
    size: u32,
    incurs_seek_penalty: u8,
}

// STORAGE_PROTOCOL_SPECIFIC_DATA(NVMe 健康日志查询用)
#[repr(C)]
struct ProtocolSpecificData {
    protocol_type: u32,  // 3 = NVMe
    data_type: u32,      // 2 = LogPage
    request_value: u32,  // 2 = Health log
    request_sub_value: u32,
    offset: u32,
    length: u32,
    fixed_return: u32,
    reserved: [u32; 3],  // 规范要求,总长 40 字节
}

const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;
const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
const PROPERTY_STANDARD_QUERY: u32 = 0;
const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_HEALTH_LOG_ID: u32 = 2;

const fn ctl_code(dev: u32, func: u32, method: u32, access: u32) -> u32 {
    (dev << 16) | (access << 14) | (func << 2) | method
}
const FILE_DEVICE_MASS_STORAGE: u32 = 0x0000_002D;
const FILE_DEVICE_DISK: u32 = 0x0000_0007;
const IOCTL_STORAGE_QUERY_PROPERTY: u32 =
    ctl_code(FILE_DEVICE_MASS_STORAGE, 0x0500, 0, 0); // METHOD_BUFFERED, FILE_ANY_ACCESS
const IOCTL_DISK_PERFORMANCE: u32 = ctl_code(FILE_DEVICE_DISK, 0x0008, 0, 0);

/// DISK_PERFORMANCE(x64,88 字节,含尾部填充)
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DiskPerformance {
    bytes_read: i64,
    bytes_written: i64,
    read_time: i64,
    write_time: i64,
    idle_time: i64,
    read_count: u32,
    write_count: u32,
    queue_depth: u32,
    split_count: u32,
    query_time: i64,
    storage_device_number: u32,
    storage_manager_name: [u16; 8],
    pad: [u8; 4],
}

struct DiskIo {
    handle: windows::Win32::Foundation::HANDLE,
}

/// SCSI_PASS_THROUGH(winnt.h,x64 布局,sizeof = 56)。
#[repr(C)]
struct ScsiPassThrough {
    length: u16,
    scsi_status: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_length: u8,
    data_in: u8,
    data_transfer_length: u32,
    time_out_value: u32,
    data_buffer_offset: u64,
    sense_info_offset: u32,
    cdb: [u8; 16],
}

/// CTL_CODE(FILE_DEVICE_CONTROLLER, 0x0401, METHOD_BUFFERED, FILE_READ|FILE_WRITE)
const IOCTL_SCSI_PASS_THROUGH: u32 = 0x4D004;

impl DiskIo {
    fn open(index: u32) -> Result<DiskIo, String> {
        let path = format!("\\\\.\\PhysicalDrive{index}");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            // desired access = 0:非管理员也能打开,QUERY_PROPERTY / 协议命令足够
            let h = CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .map_err(|e| format!("打开 {path} 失败: {e}"))?;
            Ok(DiskIo { handle: h })
        }
    }

    fn query_property(&self, query: &StoragePropertyQuery, out_size: usize) -> Result<Vec<u8>, String> {
        unsafe {
            let mut out = vec![0u8; out_size];
            let mut ret = 0u32;
            DeviceIoControl(
                self.handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                Some(query as *const _ as *mut _),
                std::mem::size_of::<StoragePropertyQuery>() as u32,
                Some(out.as_mut_ptr() as *mut _),
                out.len() as u32,
                Some(&mut ret),
                None,
            )
            .map_err(|e| format!("IOCTL_STORAGE_QUERY_PROPERTY 失败: {e}"))?;
            out.truncate(ret as usize);
            Ok(out)
        }
    }

    fn bus_type_name(&self) -> String {
        let Ok(out) = self.query_property(
            &StoragePropertyQuery {
                property_id: STORAGE_DEVICE_PROPERTY,
                query_type: PROPERTY_STANDARD_QUERY,
                additional: [0; 64],
            },
            4096,
        ) else {
            return String::new();
        };
        if out.len() < 36 {
            return String::new();
        }
        // STORAGE_DEVICE_DESCRIPTOR:BusType 位于偏移 28
        // STORAGE_BUS_TYPE(ntddstor.h):0 Unknown,1 SCSI,2 ATAPI,3 ATA,
        // 4 IEEE1394,5 SSA,6 FibreChannel,7 USB,8 RAID,9 iSCSI,10 SAS,
        // 11 SATA,12 SD,13 MMC,14 Virtual,15 FileBackedVirtual,16 Spaces,
        // 17 NVMe,18 SCM,19 Ufs
        let bus = out[28];
        match bus {
            1 => "SCSI",
            2 => "ATAPI",
            3 => "ATA",
            4 => "IEEE1394",
            5 => "SSA",
            6 => "FibreChannel",
            7 => "USB",
            8 => "RAID",
            9 => "iSCSI",
            10 => "SAS",
            11 => "SATA",
            12 => "SD",
            13 => "MMC",
            14 => "Virtual",
            15 => "FileBackedVirtual",
            16 => "Spaces",
            17 => "NVMe",
            18 => "SCM",
            19 => "UFS",
            _ => "",
        }
        .to_string()
    }

    fn is_ssd(&self) -> Option<bool> {
        let out = self
            .query_property(
                &StoragePropertyQuery {
                    property_id: STORAGE_DEVICE_SEEK_PENALTY_PROPERTY,
                    query_type: PROPERTY_STANDARD_QUERY,
                    additional: [0; 64],
                },
                64,
            )
            .ok()?;
        if out.len() < std::mem::size_of::<DeviceSeekPenaltyDescriptor>() {
            return None;
        }
        Some(out[8] == 0) // IncursSeekPenalty == 0 → SSD
    }

    /// NVMe 健康日志 → (温度°C, 磨损%, 已写入 TB, 可用备件%)
    fn nvme_health(&self) -> Option<(f64, f64, f64, f64)> {
        const DATA_OFFSET: u32 = std::mem::size_of::<ProtocolSpecificData>() as u32;
        let mut additional = [0u8; 64];
        let psd = ProtocolSpecificData {
            protocol_type: PROTOCOL_TYPE_NVME,
            data_type: NVME_DATA_TYPE_LOG_PAGE,
            request_value: NVME_HEALTH_LOG_ID,
            request_sub_value: 0,
            offset: DATA_OFFSET,
            length: 512,
            fixed_return: 0,
            reserved: [0; 3],
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &psd as *const ProtocolSpecificData as *const u8,
                additional.as_mut_ptr(),
                std::mem::size_of::<ProtocolSpecificData>(),
            );
        }
        let query = StoragePropertyQuery {
            property_id: STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY,
            // 协议专属查询也走 PropertyStandardQuery(QueryType=2 是 MaskQuery,
            // 会得到 ERROR_NOT_SUPPORTED)
            query_type: PROPERTY_STANDARD_QUERY,
            additional,
        };
        let out = self.query_property(&query, 8192).ok()?;
        // 返回缓冲:STORAGE_PROTOCOL_DATA_DESCRIPTOR{Version(4), Size(4),
        // ProtocolSpecificData(40)} + 数据(位于 ProtocolDataOffset 处)
        let data_start = 8 + DATA_OFFSET as usize;
        // 最远读到 log[55](Data Units Written = 48..63)
        if out.len() < data_start + 56 {
            return None;
        }
        let log = &out[data_start..];
        let temp_k = u16::from_le_bytes([log[1], log[2]]) as f64;
        let spare = log[3] as f64;
        let used_pct = log[5] as f64;
        // NVMe SMART/Health 日志布局:Data Units Read = 32..48,
        // Data Units Written = 48..64(128 位 LE,取低 64 位)
        let units_written = u64::from_le_bytes([
            log[48], log[49], log[50], log[51], log[52], log[53], log[54], log[55],
        ]) as f64;
        let temp_c = if temp_k > 0.0 { temp_k - 273.15 } else { f64::NAN };
        let written_tb = units_written * 512_000.0 / 1024.0 / 1024.0 / 1024.0 / 1024.0;
        Some((temp_c, used_pct, written_tb, spare))
    }

    /// 打开带读写权限的物理盘句柄。SCSI 透传需要写权限,通常要求管理员。
    fn open_rw(index: u32) -> Result<DiskIo, windows::core::Error> {
        let path = format!("\\\\.\\PhysicalDrive{index}");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        unsafe {
            let h = CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                (GENERIC_READ.0 | GENERIC_WRITE.0) as u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )?;
            Ok(DiskIo { handle: h })
        }
    }

    /// SATA SMART 温度:ATA PASS-THROUGH(12) DATA-IN 读 512B 属性区。
    /// 访问被拒(非管理员)→ NO_DRIVER;透传失败/无有效温度 → NOT_SUPPORTED。
    fn smart_temp_sata(index: u32) -> Result<f64, Status> {
        const SPT_SIZE: usize = std::mem::size_of::<ScsiPassThrough>(); // 56
        const SENSE_SIZE: usize = 32;
        const DATA_SIZE: usize = 512;
        let io = DiskIo::open_rw(index).map_err(|e| {
            if (e.code().0 & 0xFFFF) == 5 {
                Status::NoDriver
            } else {
                Status::NotSupported
            }
        })?;

        let mut cdb = [0u8; 12];
        cdb[0] = 0xA1; // ATA PASS-THROUGH(12)
        // cdb[1] bits 3:1 = protocol(4 = PIO Data-In),bit 0 = EXTEND
        // (48 位寻址标志,12 字节 CDB 必须为 0)—— 不是 T_DIR
        cdb[1] = 4 << 1;
        // cdb[2] bit5 = CK_COND,bit3 = T_DIR(1 = 从设备读出),
        // bit2 = BYT_BLOK,bits 1:0 = T_LENGTH(2 = 按扇区计数字段传输)
        cdb[2] = (1 << 5) | (1 << 3) | (1 << 2) | 2;
        cdb[3] = 0xD0; // SMART 子命令:READ DATA
        cdb[4] = 0x01; // 扇区数
        cdb[5] = 0x4F; // SMART 签名低
        cdb[6] = 0xC2; // SMART 签名高
        cdb[7] = 0xA0; // device
        cdb[8] = 0xB0; // SMART
        let mut cdb16 = [0u8; 16];
        cdb16[..12].copy_from_slice(&cdb);

        let mut buf = vec![0u8; SPT_SIZE + SENSE_SIZE + DATA_SIZE];
        let spt = ScsiPassThrough {
            length: SPT_SIZE as u16,
            scsi_status: 0,
            path_id: 0,
            target_id: 0,
            lun: 0,
            cdb_length: 12,
            sense_info_length: SENSE_SIZE as u8,
            data_in: 1, // SCSI_IOCTL_DATA_IN
            data_transfer_length: DATA_SIZE as u32,
            time_out_value: 3,
            data_buffer_offset: (SPT_SIZE + SENSE_SIZE) as u64,
            sense_info_offset: SPT_SIZE as u32,
            cdb: cdb16,
        };
        unsafe {
            std::ptr::copy_nonoverlapping(
                &spt as *const ScsiPassThrough as *const u8,
                buf.as_mut_ptr(),
                SPT_SIZE,
            );
            let mut ret = 0u32;
            DeviceIoControl(
                io.handle,
                IOCTL_SCSI_PASS_THROUGH,
                Some(buf.as_mut_ptr() as *mut _),
                buf.len() as u32,
                Some(buf.as_mut_ptr() as *mut _),
                buf.len() as u32,
                Some(&mut ret),
                None,
            )
            .map_err(|_| Status::NotSupported)?;
        }
        // SCSI 状态 0 = GOOD,2 = CHECK CONDITION(CK_COND 正常现象)
        let status = buf[2];
        if status != 0 && status != 2 {
            return Err(Status::NotSupported);
        }
        let data_start = SPT_SIZE + SENSE_SIZE;
        parse_smart_temp(&buf[data_start..]).ok_or(Status::NotSupported)
    }

    /// DISK_PERFORMANCE 累计计数(驱动自设备启动起累计;首次调用后才开始累计)。
    fn disk_performance(&self) -> Option<DiskPerformance> {
        unsafe {
            let mut out = [0u8; 128];
            let mut ret = 0u32;
            DeviceIoControl(
                self.handle,
                IOCTL_DISK_PERFORMANCE,
                None,
                0,
                Some(out.as_mut_ptr() as *mut _),
                out.len() as u32,
                Some(&mut ret),
                None,
            )
            .ok()?;
            if (ret as usize) < std::mem::size_of::<DiskPerformance>() {
                return None;
            }
            // 缓冲区 1 字节对齐,DISK_PERFORMANCE 含 u64 字段——必须按非对齐读
            Some(std::ptr::read_unaligned(out.as_ptr() as *const DiskPerformance))
        }
    }
}

impl Drop for DiskIo {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Foundation::CloseHandle(self.handle).ok();
        }
    }
}

struct DiskState {
    /// 磁盘序号 → 上一次 DISK_PERFORMANCE 采样(差分算速率)
    perf: HashMap<u32, DiskPerfSample>,
    /// 刷新计数:WMI SMART 查询较慢,已拿到温度的设备每 16 轮才复查
    temp_tick: u32,
}

#[derive(Clone, Copy)]
struct DiskPerfSample {
    bytes_read: i64,
    bytes_written: i64,
    /// ReadTime + WriteTime(100ns 单位,驱动累计)
    active_100ns: u64,
    at: Instant,
}

pub struct DiskProvider {
    state: Mutex<DiskState>,
}

impl DiskProvider {
    pub fn new() -> DiskProvider {
        DiskProvider {
            state: Mutex::new(DiskState {
                perf: HashMap::new(),
                temp_tick: 0,
            }),
        }
    }

    /// SATA SMART 属性 194/190 → 温度。
    /// 注意:该类在 root\WMI 命名空间,非管理员查询通常返回 0 行。
    fn smart_temp_via_wmi(pnp_id: &str) -> Option<f64> {
        let conn = crate::platform::windows::shared_wmi_root_wmi().ok()?;
        let rows = match conn.raw_query::<WmiSmartData>(
            "SELECT InstanceName, VendorSpecific FROM MSStorageDriver_FailurePredictData",
        ) {
            Ok(r) => r,
            Err(e) => {
                crate::logbuf::info(format!("SMART WMI 查询失败: {e}"));
                return None;
            }
        };
        // InstanceName 与 Win32_DiskDrive 的 PNPDeviceID 格式不同(大小写/后缀),
        // 用末段硬件实例串匹配:两边都含 "4&58dfa2f&0&000000" 这一段。
        let tail = pnp_id
            .rsplit(['\\', '/'])
            .next()?
            .to_ascii_lowercase();
        for row in rows {
            let Some(inst) = row.InstanceName.as_deref() else { continue };
            if !inst.to_ascii_lowercase().contains(&tail) {
                continue;
            }
            let Some(data) = row.VendorSpecific else { continue };
            if let Some(t) = parse_smart_temp(&data) {
                return Some(t);
            }
        }
        None
    }
}

/// 从 SMART READ DATA(WMI VendorSpecific 同构,362B)解析温度:
/// 偏移 2 起 12B/条:id(1) flags(2) value(1) raw(8)。优先 194,其次 190;
/// 温度取值依次尝试:raw 低 32 位(多数盘 raw=temp)→ raw 字节 1(三星/希捷
/// 把当前温度放这里,byte 0 是历史最高温)→ raw 字节 6(个别大端固件)→
/// 归一化 value(老盘 normalized=temp);全部要求 1..=120 的合理范围。
fn parse_smart_temp(data: &[u8]) -> Option<f64> {
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
        let candidates = [le32, raw[1] as u32, raw[6] as u32, value];
        candidates.into_iter().find(|t| (1..=120).contains(t))
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

impl Provider for DiskProvider {
    fn describe(&self) -> &str {
        "win-disk"
    }

    fn category(&self) -> Category {
        Category::Disk
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let conn = shared_wmi()?;
        let drives = conn
            .raw_query::<WmiDiskDrive>(
                "SELECT Index, Model, SerialNumber, Size, PNPDeviceID, MediaType, InterfaceType FROM Win32_DiskDrive",
            )
            .map_err(|e| format!("WMI Win32_DiskDrive 查询失败: {e}"))?;
        if drives.is_empty() {
            return Err("未发现物理磁盘".into());
        }

        let mut devices = Vec::new();
        for d in &drives {
            let Some(index) = d.Index else { continue };
            let model = d.Model.clone().unwrap_or_default();
            let mut b = DeviceBuilder::new(
                format!("disk.pd{index}"),
                model.clone(),
            )
            .vendor("Storage")
            .model(model.clone());

            if let Some(serial) = &d.SerialNumber {
                if !serial.is_empty() {
                    b = b.serial(serial.trim().to_string());
                }
            }

            let io = DiskIo::open(index).ok();
            let mut nvme = None;
            if let Some(io) = &io {
                let bus = io.bus_type_name();
                if !bus.is_empty() {
                    b = b.static_str("bus_type", "Bus Type", bus.clone());
                }
                if let Some(ssd) = io.is_ssd() {
                    b = b.static_str(
                        "media_type",
                        "Media Type",
                        if ssd { "SSD" } else { "HDD" },
                    );
                }
                nvme = io.nvme_health();
            }
            let pnp = d.PNPDeviceID.clone().unwrap_or_default();
            if !pnp.is_empty() {
                b = b.static_str("pnp_id", "PNP Device ID", pnp.clone());
            }
            if let Some(size) = d.Size {
                if size > 0 {
                    b = b.static_f64("capacity_gb", "Capacity", "GB", crate::util::bytes_to_gb(size));
                }
            }

            // 温度/磨损/写入量
            let temp_source: Option<(String, f64)> = match nvme {
                Some((t, _, _, _)) if t.is_finite() => Some(("nvme".into(), t)),
                _ => Self::smart_temp_via_wmi(&pnp).map(|t| ("smart".into(), t)),
            };
            match temp_source {
                Some((src, t)) => {
                    let _ = src;
                    b = b.dynamic_now("temperature_c", "Temperature", "°C", Value::F64((t * 10.0).round() / 10.0));
                }
                None => {
                    b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64);
                }
            }
            if let Some((_, used_pct, written_tb, spare)) = nvme {
                if used_pct > 0.0 {
                    b = b.dynamic_now("wear_pct", "Percentage Used", "%", Value::F64(used_pct));
                }
                if written_tb > 0.0 {
                    b = b.dynamic_now("written_tb", "Total Written", "TB", Value::F64((written_tb * 100.0).round() / 100.0));
                }
                if spare > 0.0 {
                    b = b.dynamic_now("spare_pct", "Available Spare", "%", Value::F64(spare));
                }
            }

            b = b.dynamic("activity_pct", "Activity", "%", ValueType::F64)
                .dynamic("read_kbps", "Read Rate", "KB/s", ValueType::F64)
                .dynamic("write_kbps", "Write Rate", "KB/s", ValueType::F64);
            devices.push(b.build());
        }

        Ok(devices)
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        // 性能:IOCTL_DISK_PERFORMANCE 差分
        for dev in devices.iter_mut() {
            let Some(index) = dev
                .meta
                .id
                .trim_start_matches("disk.pd")
                .parse::<u32>()
                .ok()
            else {
                continue;
            };
            let Ok(io) = DiskIo::open(index) else {
                continue;
            };
            let Some(p) = io.disk_performance() else {
                continue;
            };
            let now = Instant::now();
            let sample = DiskPerfSample {
                bytes_read: p.bytes_read,
                bytes_written: p.bytes_written,
                active_100ns: p
                    .read_time
                    .max(0)
                    .saturating_add(p.write_time.max(0)) as u64,
                at: now,
            };
            let prev = {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.perf.insert(index, sample)
            };
            if let Some(prev) = prev {
                let dt = now.duration_since(prev.at).as_secs_f64();
                if dt < 0.05 {
                    continue;
                }
                let d_read = p.bytes_read.saturating_sub(prev.bytes_read);
                let d_write = p.bytes_written.saturating_sub(prev.bytes_written);
                let d_active_100ns = sample.active_100ns.saturating_sub(prev.active_100ns);
                let active_pct = (d_active_100ns as f64 * 100.0 / dt / 1e7).clamp(0.0, 100.0);
                dev.set_reading(
                    "read_kbps",
                    Reading::now(Value::F64(
                        (d_read as f64 / dt / 1024.0 * 10.0).round() / 10.0,
                    )),
                );
                dev.set_reading(
                    "write_kbps",
                    Reading::now(Value::F64(
                        (d_write as f64 / dt / 1024.0 * 10.0).round() / 10.0,
                    )),
                );
                dev.set_reading(
                    "activity_pct",
                    Reading::now(Value::F64((active_pct * 10.0).round() / 10.0)),
                );
            }
        }

        // 温度刷新:NVMe 走 IOCTL(快,每轮可读);SATA 走 WMI SMART/ATA 透传
        // (慢且需管理员),拿到值后每 16 轮才复查一次,终态(不支持/需要权限)不再重试。
        let temp_tick = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.temp_tick = st.temp_tick.wrapping_add(1);
            st.temp_tick
        };
        let pnp_by_id: HashMap<String, String> = {
            let mut m = HashMap::new();
            for dev in devices.iter() {
                if let Some(crate::model::SensorData {
                    current: crate::model::Reading { value: Value::Str(s), .. },
                    ..
                }) = dev.sensors.iter().find(|s| s.desc.code == "pnp_id")
                {
                    m.insert(dev.meta.id.clone(), s.clone());
                }
            }
            m
        };
        for dev in devices.iter_mut() {
            let idx = dev.meta.id.trim_start_matches("disk.pd");
            let Ok(index) = idx.parse::<u32>() else { continue };
            let temp_sensor = dev
                .sensors
                .iter()
                .find(|s| s.desc.code == "temperature_c")
                .map(|s| (s.desc.kind, s.current.status));
            let Some((kind, status)) = temp_sensor else { continue };
            if kind != SensorKind::Dynamic {
                continue;
            }
            // 终态(不支持/需要管理员)不再反复尝试;已有值的 SATA 盘降频复查
            match status {
                Status::NotSupported | Status::NoDriver => continue,
                Status::Ok if temp_tick % 16 != 0 => {
                    // NVMe 温度走 IOCTL,代价低,每轮刷新;SATA 温度复查降频
                    let is_nvme = dev.sensors.iter().any(|s| s.desc.code == "wear_pct");
                    if !is_nvme {
                        continue;
                    }
                }
                _ => {}
            }
            let Ok(io) = DiskIo::open(index) else { continue };
            if let Some((t, used_pct, written_tb, spare)) = io.nvme_health() {
                if t.is_finite() {
                    dev.set_reading("temperature_c", Reading::now(Value::F64((t * 10.0).round() / 10.0)));
                }
                // 磨损/备件/累计写入:变化慢,但必须持续刷新,否则冻结在 probe 值
                if used_pct > 0.0 && dev.sensors.iter().any(|s| s.desc.code == "wear_pct") {
                    dev.set_reading("wear_pct", Reading::now(Value::F64(used_pct)));
                }
                if written_tb > 0.0 && dev.sensors.iter().any(|s| s.desc.code == "written_tb") {
                    dev.set_reading(
                        "written_tb",
                        Reading::now(Value::F64((written_tb * 100.0).round() / 100.0)),
                    );
                }
                if spare > 0.0 && dev.sensors.iter().any(|s| s.desc.code == "spare_pct") {
                    dev.set_reading("spare_pct", Reading::now(Value::F64(spare)));
                }
                if t.is_finite() {
                    continue;
                }
            }
            if let Some(pnp) = pnp_by_id.get(&dev.meta.id) {
                if let Some(t) = Self::smart_temp_via_wmi(pnp) {
                    dev.set_reading("temperature_c", Reading::now(Value::F64((t * 10.0).round() / 10.0)));
                    continue;
                }
            }
            // USB 桥不支持 ATA 透传,直接标不支持;其余尝试一次并落到终态,
            // 不再让传感器永远停在"等待首刷"。
            let bus = dev
                .sensors
                .iter()
                .find(|s| s.desc.code == "bus_type")
                .and_then(|s| match &s.current.value {
                    Value::Str(x) => Some(x.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            if bus == "USB" {
                dev.set_reading("temperature_c", Reading::failed(Status::NotSupported, ValueType::F64));
                continue;
            }
            match DiskIo::smart_temp_sata(index) {
                Ok(t) => dev.set_reading(
                    "temperature_c",
                    Reading::now(Value::F64((t * 10.0).round() / 10.0)),
                ),
                Err(s) => dev.set_reading("temperature_c", Reading::failed(s, ValueType::F64)),
            }
        }
    }
}
