#![cfg(target_os = "windows")]
//! SATA SMART 透传诊断:IOCTL_ATA_PASS_THROUGH vs IOCTL_SCSI_PASS_THROUGH
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::IO::DeviceIoControl;

const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D1400; // CTL_CODE(0x2D, 0x500, METHOD_BUFFERED, FILE_ANY_ACCESS)

const IOCTL_ATA_PASS_THROUGH: u32 = 0x4D02C;
const IOCTL_SCSI_PASS_THROUGH: u32 = 0x4D004;

/// STORAGE_PROPERTY_QUERY(PropertyId=StorageDeviceProtocolSpecificProperty)
#[repr(C)]
#[derive(Clone, Copy)]
struct PropQuery {
    property_id: u32, // 50 = StorageDeviceProtocolSpecificProperty
    query_type: u32,  // 0 = PropertyStandardQuery
    additional: [u8; 64],
}

/// STORAGE_PROTOCOL_SPECIFIC_DATA
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProtocolSpecific {
    version: u32,
    size: u32,
    protocol_type: u32, // 2 = Ata
    data_type: u32,     // 1 = Identify, 2 = LogPage
    request_value: u32,
    request_sub_value: u32,
    offset: u32,
    length: u32,
    fixed_return: u32,
    reserved: [u32; 3],
}

/// 变体 C:协议专属查询走 ATA(IOCTL_STORAGE_QUERY_PROPERTY)
fn query_variant(index: u32, data_type: u32, req: u32, label: &str) {
    let Some(h) = open(index) else { return };
    let ps = ProtocolSpecific {
        version: 1,
        size: std::mem::size_of::<ProtocolSpecific>() as u32,
        protocol_type: 2, // Ata
        data_type,
        request_value: req,
        request_sub_value: 0,
        offset: std::mem::size_of::<ProtocolSpecific>() as u32,
        length: 512,
        fixed_return: 0,
        reserved: [0; 3],
    };
    let mut q = PropQuery {
        property_id: 50,
        query_type: 0,
        additional: [0; 64],
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            &ps as *const ProtocolSpecific as *const u8,
            q.additional.as_mut_ptr(),
            std::mem::size_of::<ProtocolSpecific>(),
        );
        let mut out = vec![0u8; 4096];
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q as *const PropQuery as *mut _),
            std::mem::size_of::<PropQuery>() as u32,
            Some(out.as_mut_ptr() as *mut _),
            out.len() as u32,
            Some(&mut ret),
            None,
        )
        .is_ok();
        if !ok {
            println!("[QRY pd{index} {label}] ioctl 失败: {}", err_str());
        } else {
            out.truncate(ret as usize);
            println!("[QRY pd{index} {label}] ok ret={ret}");
            // 返回缓冲:STORAGE_PROTOCOL_DATA_DESCRIPTOR{Version,Size,ProtocolSpecific(40)} + 数据
            let start = 8 + std::mem::size_of::<ProtocolSpecific>();
            if out.len() > start + 12 {
                dump_smart(&out[start..]);
            } else {
                println!("  返回过短: {:02X?}", &out[..out.len().min(24)]);
            }
        }
        let _ = CloseHandle(h);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AtaPassThrough {
    length: u16,
    ata_flags: u16,
    path_id: u8,
    target_id: u8,
    lun: u8,
    reserved_ata: u8,
    data_transfer_length: u32,
    time_out_value: u32,
    data_buffer_offset: u64,
    previous_task_file: [u8; 8],
    current_task_file: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
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

fn open(index: u32) -> Option<HANDLE> {
    let path = format!("\\\\.\\PhysicalDrive{index}");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        Some(
            CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                (GENERIC_READ.0 | GENERIC_WRITE.0) as u32,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                windows::Win32::Storage::FileSystem::OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .ok()?,
        )
    }
}

fn err_str() -> String {
    let e = std::io::Error::last_os_error();
    e.to_string()
}

/// SMART READ DATA(0xD0),返回 512B 数据
fn ata_variant(index: u32) {
    let Some(h) = open(index) else {
        println!("[ATA pd{index}] open 失败: {}", err_str());
        return;
    };
    const SPT: usize = std::mem::size_of::<AtaPassThrough>(); // 40
    let mut buf = vec![0u8; SPT + 512];
    let mut apt = AtaPassThrough {
        length: SPT as u16,
        ata_flags: 0x02, // ATA_FLAGS_DATA_IN
        path_id: 0,
        target_id: 0,
        lun: 0,
        reserved_ata: 0,
        data_transfer_length: 512,
        time_out_value: 3,
        data_buffer_offset: SPT as u64,
        previous_task_file: [0; 8],
        current_task_file: [0; 8],
    };
    // task file: features/sect/lba_lo/lba_mid/lba_high/device/cmd/res
    apt.current_task_file = [0xD0, 0x00, 0x00, 0x4F, 0xC2, 0xA0, 0xB0, 0x00];
    unsafe {
        std::ptr::copy_nonoverlapping(
            &apt as *const AtaPassThrough as *const u8,
            buf.as_mut_ptr(),
            SPT,
        );
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_ATA_PASS_THROUGH,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut ret),
            None,
        )
        .is_ok();
        if !ok {
            println!("[ATA pd{index}] ioctl 失败: {}", err_str());
        } else {
            let apt2 = *(buf.as_ptr() as *const AtaPassThrough);
            let data = &buf[SPT..SPT + 512];
            println!(
                "[ATA pd{index}] ok ret={ret} taskfile_out={:02X?}",
                apt2.current_task_file
            );
            dump_smart(data);
        }
        let _ = CloseHandle(h);
    }
}

fn scsi_variant(index: u32) {
    let Some(h) = open(index) else { return };
    const SPT: usize = std::mem::size_of::<ScsiPassThrough>();
    const SENSE: usize = 32;
    let mut buf = vec![0u8; SPT + SENSE + 512];
    let mut cdb = [0u8; 12];
    cdb[0] = 0xA1;
    cdb[1] = (4 << 1) | 1;
    cdb[2] = 0x20;
    cdb[3] = 0xD0;
    cdb[4] = 0x01;
    cdb[5] = 0x4F;
    cdb[6] = 0xC2;
    cdb[7] = 0xA0;
    cdb[8] = 0xB0;
    let mut cdb16 = [0u8; 16];
    cdb16[..12].copy_from_slice(&cdb);
    let spt = ScsiPassThrough {
        length: SPT as u16,
        scsi_status: 0,
        path_id: 0,
        target_id: 0,
        lun: 0,
        cdb_length: 12,
        sense_info_length: SENSE as u8,
        data_in: 1,
        data_transfer_length: 512,
        time_out_value: 3,
        data_buffer_offset: (SPT + SENSE) as u64,
        sense_info_offset: SPT as u32,
        cdb: cdb16,
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            &spt as *const ScsiPassThrough as *const u8,
            buf.as_mut_ptr(),
            SPT,
        );
        let mut ret = 0u32;
        let ok = DeviceIoControl(
            h,
            IOCTL_SCSI_PASS_THROUGH,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut ret),
            None,
        )
        .is_ok();
        if !ok {
            println!("[SCSI pd{index}] ioctl 失败: {}", err_str());
        } else {
            let scsi_status = buf[2];
            println!("[SCSI pd{index}] ok ret={ret} scsi_status={scsi_status:02X}");
            let sense = &buf[SPT..SPT + SENSE];
            if sense.iter().any(|&b| b != 0) {
                println!("  sense: {:02X?}", &sense[..18]);
            }
            dump_smart(&buf[SPT + SENSE..]);
        }
        let _ = CloseHandle(h);
    }
}

fn dump_smart(data: &[u8]) {
    if data.len() < 362 {
        println!("  数据不足 {}B,前 16B: {:02X?}", data.len(), &data[..data.len().min(16)]);
        return;
    }
    println!("  版本 {:02X} {:02X}", data[0], data[1]);
    for off in (2..362).step_by(12) {
        let id = data[off];
        if id == 0 || id == 194 || id == 190 {
            println!(
                "  attr id={:3} value={:3} raw={:02X?}",
                id,
                data[off + 3],
                &data[off + 4..off + 12]
            );
        }
    }
}

fn main() {
    let count = 4; // pd0..pd3
    for index in 0..count {
        ata_variant(index);
        scsi_variant(index);
        query_variant(index, 1, 0, "IDENTIFY"); // AtaDataTypeIdentify
        query_variant(index, 2, 0xE0, "LOG E0"); // AtaDataTypeLogPage,SMART 数据
        println!("----");
    }
}
