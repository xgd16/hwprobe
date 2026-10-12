#[cfg(target_os = "windows")]
mod win_probe {
//! 诊断:PDH 通配符计数器 + 磁盘描述符原始字节。
//! 用法: cargo run --release -p hwprobe-cli --example dbg


use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

#[repr(C)]
struct StoragePropertyQuery {
    property_id: u32,
    query_type: u32,
    additional: [u8; 64],
}

const fn ctl_code(dev: u32, func: u32, method: u32, access: u32) -> u32 {
    (dev << 16) | (access << 14) | (func << 2) | method
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn main() {
    unsafe {
        dbg_pd();
        dbg_disk(0);
        dbg_disk(3);
    }
}

unsafe fn dbg_pd() {
    let mut hq = PDH_HQUERY::default();
    let st = PdhOpenQueryW(None, 0, &mut hq);
    println!("[PDH] open=0x{st:08X}");
    let path = wide("Processor(*)\\% Processor Time");
    let mut hc = PDH_HCOUNTER::default();
    let st = PdhAddEnglishCounterW(hq, windows::core::PCWSTR(path.as_ptr()), 0, &mut hc);
    println!("[PDH] add_english=0x{st:08X}");
    if st != 0 {
        return;
    }
    let st = PdhCollectQueryData(hq);
    println!("[PDH] collect#1=0x{st:08X}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let st = PdhCollectQueryData(hq);
    println!("[PDH] collect#2=0x{st:08X}");

    let mut size: u32 = 0;
    let mut count: u32 = 0;
    let st1 = PdhGetFormattedCounterArrayW(hc, PDH_FMT_DOUBLE, &mut size, &mut count, None);
    println!("[PDH] array(size-query) status=0x{st1:08X} size={size} count={count}");
    if size == 0 {
        return;
    }
    let mut buf = vec![0usize; size as usize / 8 + 1];
    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
    count = 0;
    let st2 = PdhGetFormattedCounterArrayW(hc, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items));
    println!("[PDH] array(read) status=0x{st2:08X} count={count}");
    for i in 0..count.min(4) as usize {
        let item = &*items.add(i);
        println!(
            "  item[{i}] cstatus=0x{:08X} name={:?} value={:?}",
            item.FmtValue.CStatus,
            item.szName.to_string(),
            item.FmtValue.Anonymous.doubleValue
        );
    }
    let _ = PdhCloseQuery(hq);
}

unsafe fn dbg_disk(index: u32) {
    let path = wide(&format!("\\\\.\\PhysicalDrive{index}"));
    let h = match CreateFileW(
        windows::core::PCWSTR(path.as_ptr()),
        0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        windows::Win32::Storage::FileSystem::OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    ) {
        Ok(h) => h,
        Err(e) => {
            println!("[DISK{index}] open 失败: {e}");
            return;
        }
    };
    let mut q = StoragePropertyQuery {
        property_id: 0,
        query_type: 0,
        additional: [0; 64],
    };
    let mut out = vec![0u8; 4096];
    let mut ret = 0u32;
    let r = DeviceIoControl(
        h,
        ctl_code(0x2D, 0x500, 0, 0),
        Some(&mut q as *mut _ as *mut _),
        std::mem::size_of::<StoragePropertyQuery>() as u32,
        Some(out.as_mut_ptr() as *mut _),
        out.len() as u32,
        Some(&mut ret),
        None,
    );
    println!("[DISK{index}] descriptor ioctl ok={r:?} ret={ret}");
    if r.is_ok() {
        let hex: Vec<String> = out[..ret.min(40) as usize].iter().map(|b| format!("{b:02X}")).collect();
        println!("  head: {}", hex.join(" "));
        println!("  byte[28] (BusType) = {}", out[28]);
    }

    // NVMe 协议查询
    let mut q2 = StoragePropertyQuery {
        property_id: 50,
        query_type: 2,
        additional: [0; 64],
    };
    // STORAGE_PROTOCOL_SPECIFIC_DATA: type=3, datatype=2, reqval=2, sub=0, offset=40, len=512, fixed=0, resv[3]
    let fields: [u32; 10] = [3, 2, 2, 0, 40, 512, 0, 0, 0, 0];
    std::ptr::copy_nonoverlapping(
        fields.as_ptr() as *const u8,
        q2.additional.as_mut_ptr(),
        40,
    );
    let mut out2 = vec![0u8; 8192];
    let mut ret2 = 0u32;
    let r2 = DeviceIoControl(
        h,
        ctl_code(0x2D, 0x500, 0, 0),
        Some(&mut q2 as *mut _ as *mut _),
        std::mem::size_of::<StoragePropertyQuery>() as u32,
        Some(out2.as_mut_ptr() as *mut _),
        out2.len() as u32,
        Some(&mut ret2),
        None,
    );
    println!("[DISK{index}] nvme-health ioctl ok={r2:?} ret={ret2} err={:?}", r2.as_ref().err().map(|e| e.code().0));
    if r2.is_ok() && ret2 >= 48 + 12 {
        let log = &out2[48..];
        println!(
            "  nvme: tempK={} spare={} used%={} units_written={}",
            u16::from_le_bytes([log[1], log[2]]),
            log[3],
            log[5],
            u64::from_le_bytes([log[40], log[41], log[42], log[43], log[44], log[45], log[46], log[47]])
        );
    }
    windows::Win32::Foundation::CloseHandle(h).ok();
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
