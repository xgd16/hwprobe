#[cfg(target_os = "windows")]
mod win_probe {
//! 磁盘 SMART 拿取通路实测(免提权 vs 需提权)。
//!
//! 背景:provider 的 SATA 温度走 IOCTL_SCSI_PASS_THROUGH(需 GENERIC_READ|WRITE,
//! 非管理员 → 拒绝访问 → NO_DRIVER)和 root\WMI MSStorageDriver_FailurePredictData
//! (非管理员 → 0x80041003),两条路都要提权。而 NVMe 健康日志走的是
//! IOCTL_STORAGE_QUERY_PROPERTY + STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY,
//! 句柄 desired access = 0,**免提权**就能读到。
//!
//! 疑问:同一条查询接口把 ProtocolType 换成 ATA(2)、DataType 换成 LogPage(2),
//! 是否也能免提权读到 SATA 的 SMART 属性表?本程序在非管理员权限下实测,
//! 并顺带把 NVMe 健康日志的**全部**字段解出来(看还有哪些现成可加的读数)。
//!
//! 用法: dbg20

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

// CTL_CODE(FILE_DEVICE_MASS_STORAGE=0x2D, 0x0500, METHOD_BUFFERED, FILE_ANY_ACCESS)
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x2D1400;
const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
const PROPERTY_STANDARD_QUERY: u32 = 0;

const PROTOCOL_TYPE_ATA: u32 = 2;
const PROTOCOL_TYPE_NVME: u32 = 3;
const ATA_DATA_TYPE_IDENTIFY: u32 = 1;
const ATA_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_HEALTH_LOG_ID: u32 = 2;

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ProtocolSpecificData {
    protocol_type: u32,
    data_type: u32,
    request_value: u32,
    request_sub_value: u32,
    offset: u32,
    length: u32,
    fixed_return: u32,
    reserved: [u32; 3],
}

const DATA_OFFSET: usize = std::mem::size_of::<ProtocolSpecificData>(); // 40

/// 一次协议专属查询。返回 (错误码, 返回缓冲);错误码 0 表示成功。
unsafe fn proto_query(
    handle: windows::Win32::Foundation::HANDLE,
    protocol: u32,
    data_type: u32,
    request_value: u32,
    offset: u32,
    length: u32,
) -> (u32, Vec<u8>) {
    let psd = ProtocolSpecificData {
        protocol_type: protocol,
        data_type,
        request_value,
        request_sub_value: 0,
        offset: DATA_OFFSET as u32 + offset,
        length,
        fixed_return: 0,
        reserved: [0; 3],
    };
    let mut additional = [0u8; 64];
    std::ptr::copy_nonoverlapping(
        &psd as *const _ as *const u8,
        additional.as_mut_ptr(),
        DATA_OFFSET,
    );
    let query = StoragePropertyQuery {
        property_id: STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY,
        query_type: PROPERTY_STANDARD_QUERY,
        additional,
    };
    let mut out = vec![0u8; 8192];
    let mut ret = 0u32;
    let r = DeviceIoControl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(&query as *const _ as *mut _),
        std::mem::size_of::<StoragePropertyQuery>() as u32,
        Some(out.as_mut_ptr() as *mut _),
        out.len() as u32,
        Some(&mut ret),
        None,
    );
    match r {
        Ok(()) => {
            out.truncate(ret as usize);
            (0, out)
        }
        Err(e) => {
            let hr = e.code().0 as u32;
            let code = if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr };
            (code, Vec::new())
        }
    }
}

fn open_drive(index: u32) -> Result<windows::Win32::Foundation::HANDLE, u32> {
    let path = format!("\\\\.\\PhysicalDrive{index}");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        // desired access = 0:非管理员也能打开。
        match CreateFileW(
            windows::core::PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            Ok(h) => Ok(h),
            Err(e) => {
                let hr = e.code().0 as u32;
                Err(if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr })
            }
        }
    }
}

/// 总线类型查询:additional 必须全零(带了协议结构会得到 INVALID_FUNCTION)。
unsafe fn bus_type(handle: windows::Win32::Foundation::HANDLE) -> Option<u8> {
    let query = StoragePropertyQuery {
        property_id: STORAGE_DEVICE_PROPERTY,
        query_type: PROPERTY_STANDARD_QUERY,
        additional: [0; 64],
    };
    let mut out = vec![0u8; 1024];
    let mut ret = 0u32;
    DeviceIoControl(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(&query as *const _ as *mut _),
        std::mem::size_of::<StoragePropertyQuery>() as u32,
        Some(out.as_mut_ptr() as *mut _),
        out.len() as u32,
        Some(&mut ret),
        None,
    )
    .map_err(|e| {
        let hr = e.code().0 as u32;
        let c = if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr };
        println!("      总线查询失败 code={c} ({e})");
    })
    .ok()?;
    println!("      总线查询成功 ret={ret} out[28]={}", out[28]);
    if (ret as usize) < 36 {
        return None;
    }
    Some(out[28])
}

fn bus_name(b: u8) -> &'static str {
    match b {
        3 => "ATA",
        7 => "USB",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        17 => "NVMe",
        19 => "UFS",
        _ => "其他",
    }
}

/// ATA SMART 属性表 → 打印关心的几条(ID 190/194 温度,231 SSD 寿命)。
fn dump_smart_attrs(data: &[u8]) {
    if data.len() < 2 + 30 * 12 {
        println!("      数据不足 {} 字节", data.len());
        return;
    }
    for i in 0..30 {
        let a = &data[2 + i * 12..2 + i * 12 + 12];
        let id = a[0];
        if id == 0 {
            continue;
        }
        let value = a[3];
        let raw = u64::from_le_bytes([a[5], a[6], a[7], a[8], a[9], a[10], 0, 0]);
        if matches!(id, 5 | 9 | 12 | 190 | 194 | 197 | 198 | 231 | 233 | 241 | 242) {
            println!("      属性 {id:>3}: 归一值={value:>3} 原始={raw}");
        }
    }
}

/// NVMe SMART/Health 日志(512B)全字段。
fn dump_nvme_health(log: &[u8]) {
    if log.len() < 232 {
        println!("      数据不足 {} 字节", log.len());
        return;
    }
    let u16le = |o: usize| u16::from_le_bytes([log[o], log[o + 1]]) as u64;
    let u128lo = |o: usize| {
        u128::from_le_bytes([
            log[o], log[o + 1], log[o + 2], log[o + 3], log[o + 4], log[o + 5], log[o + 6],
            log[o + 7], log[o + 8], log[o + 9], log[o + 10], log[o + 11], log[o + 12],
            log[o + 13], log[o + 14], log[o + 15],
        ])
    };
    let temp_k = u16le(1);
    let temp_c = if temp_k > 0 { temp_k as f64 - 273.15 } else { f64::NAN };
    println!("      Critical Warning        = 0x{:02X}", log[0]);
    println!("      Temperature             = {temp_c:.1} °C");
    println!("      Available Spare         = {} %  (阈值 {} %)", log[3], log[4]);
    println!("      Percentage Used         = {} %", log[5]);
    println!("      Data Units Read         = {} (×512000 B = {:.2} TB)", u128lo(32), u128lo(32) as f64 * 512_000.0 / 1e12);
    println!("      Data Units Written      = {} (×512000 B = {:.2} TB)", u128lo(48), u128lo(48) as f64 * 512_000.0 / 1e12);
    println!("      Host Read Commands      = {}", u128lo(64));
    println!("      Host Write Commands     = {}", u128lo(80));
    println!("      Controller Busy Time    = {} 分钟", u128lo(96));
    println!("      Power Cycles            = {}", u128lo(112));
    println!("      Power On Hours          = {} 小时", u128lo(128));
    println!("      Unsafe Shutdowns        = {}", u128lo(144));
    println!("      Media Errors            = {}", u128lo(160));
    println!("      Error Log Entries       = {}", u128lo(176));
    println!("      Warning  Temp Time      = {} 分钟", u16le(192) | (u16le(194) << 16));
    println!("      Critical Temp Time      = {} 分钟", u16le(196) | (u16le(198) << 16));
    println!("      Temp Sensor 1 / 2       = {} K / {} K", u16le(200), u16le(202));
}

pub fn main() {
    println!("== 免提权(句柄 desired access = 0)的协议专属查询 ==");
    println!("DATA_OFFSET = {DATA_OFFSET}(STORAGE_PROTOCOL_DATA_DESCRIPTOR 头 48 字节)\n");

    for index in 0..8u32 {
        let handle = match open_drive(index) {
            Ok(h) => h,
            Err(2) => break, // 没有更多物理盘
            Err(code) => {
                println!("[PhysicalDrive{index}] 打开失败 code={code}");
                continue;
            }
        };
        unsafe {
            // 总线类型(STORAGE_DEVICE_DESCRIPTOR 偏移 28)
            let bus = bus_type(handle).unwrap_or(0xFF);
            println!("[PhysicalDrive{index}] 总线 = {}", bus_name(bus));

            if bus == 11 || bus == 3 {
                // SATA/ATA:试 ATA 协议专属查询
                for (label, data_type, req, sub) in [
                    ("LogPage req=0x00 日志目录", ATA_DATA_TYPE_LOG_PAGE, 0x00, 0u32),
                    ("LogPage req=0x02 综合错误日志", ATA_DATA_TYPE_LOG_PAGE, 0x02, 0),
                    ("LogPage req=0x03 扩展综合错误日志", ATA_DATA_TYPE_LOG_PAGE, 0x03, 0),
                    ("LogPage req=0x04 设备统计", ATA_DATA_TYPE_LOG_PAGE, 0x04, 0),
                    ("LogPage req=0x06 自检日志", ATA_DATA_TYPE_LOG_PAGE, 0x06, 0),
                    ("LogPage req=0x09 选择性自检", ATA_DATA_TYPE_LOG_PAGE, 0x09, 0),
                    ("LogPage req=0x11 SATA 物理事件", ATA_DATA_TYPE_LOG_PAGE, 0x11, 0),
                    ("Identify", ATA_DATA_TYPE_IDENTIFY, 0x00, 0),
                ] {
                    let mut psd = ProtocolSpecificData {
                        protocol_type: PROTOCOL_TYPE_ATA,
                        data_type,
                        request_value: req,
                        request_sub_value: sub,
                        offset: DATA_OFFSET as u32,
                        length: 512,
                        fixed_return: 0,
                        reserved: [0; 3],
                    };
                    let mut additional = [0u8; 64];
                    std::ptr::copy_nonoverlapping(
                        &psd as *const _ as *const u8,
                        additional.as_mut_ptr(),
                        DATA_OFFSET,
                    );
                    let query = StoragePropertyQuery {
                        property_id: STORAGE_DEVICE_PROTOCOL_SPECIFIC_PROPERTY,
                        query_type: PROPERTY_STANDARD_QUERY,
                        additional,
                    };
                    let mut out = vec![0u8; 8192];
                    let mut ret = 0u32;
                    let r = DeviceIoControl(
                        handle,
                        IOCTL_STORAGE_QUERY_PROPERTY,
                        Some(&query as *const _ as *mut _),
                        std::mem::size_of::<StoragePropertyQuery>() as u32,
                        Some(out.as_mut_ptr() as *mut _),
                        out.len() as u32,
                        Some(&mut ret),
                        None,
                    );
                    let _ = &mut psd;
                    match r {
                        Ok(()) => {
                            let ret = ret as usize;
                            let (doff, dlen) = if ret >= 32 {
                                (
                                    u32::from_le_bytes([out[24], out[25], out[26], out[27]]),
                                    u32::from_le_bytes([out[28], out[29], out[30], out[31]]),
                                )
                            } else {
                                (0, 0)
                            };
                            let nz = out[..ret].iter().filter(|b| **b != 0).count();
                            println!(
                                "      ATA {label}: 成功 ret={ret} 描述符 offset={doff} length={dlen} 非零字节={nz}"
                            );
                            let data = &out[8 + DATA_OFFSET..ret];
                            let head: Vec<String> =
                                data.iter().take(24).map(|b| format!("{b:02X}")).collect();
                            println!("        前 24 字节: {}", head.join(" "));
                            // 设备统计日志(0x04):按 8 字节统计项列出非零项,
                            // 温度统计页(第 3 页)的 Current Temperature 就在其中。
                            if req == 0x04 && data.len() >= 8 {
                                println!("        -- 设备统计日志 8 字节项 --");
                                for (i, w) in data.chunks_exact(8).enumerate().take(64) {
                                    let v = u64::from_le_bytes([
                                        w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7],
                                    ]);
                                    if v != 0 {
                                        println!("           [{i:>2}] 0x{v:016X}  = {v}");
                                    }
                                }
                            }
                            if data.len() >= 512 && req == 0x00 {
                                dump_smart_attrs(data);
                            }
                        }
                        Err(e) => {
                            let hr = e.code().0 as u32;
                            let c = if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr };
                            println!("      ATA {label}: 失败 code={c} ({e})");
                        }
                    }
                }
            }

            if bus == 17 {
                // NVMe:健康日志全字段
                let (code, out) =
                    proto_query(handle, PROTOCOL_TYPE_NVME, NVME_DATA_TYPE_LOG_PAGE, NVME_HEALTH_LOG_ID, 0, 512);
                if code == 0 && out.len() > 8 + DATA_OFFSET {
                    let log = &out[8 + DATA_OFFSET..];
                    println!("      NVMe 健康日志 {} 字节:", log.len());
                    dump_nvme_health(log);
                } else {
                    println!("      NVMe 健康日志查询失败 code={code}");
                }
            }

            let _ = CloseHandle(handle);
        }
    }
}
}

#[cfg(target_os = "windows")]
fn main() {
    win_probe::main();
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("本探针仅支持 Windows");
}
