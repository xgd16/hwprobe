//! 诊断:哪些 PDH 计数器对象真实存在
#![cfg(target_os = "windows")]
use windows::Win32::System::Performance::*;

fn wide0(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

fn try_add(path: &str) {
    unsafe {
        let mut hq = PDH_HQUERY::default();
        if PdhOpenQueryW(None, 0, &mut hq) != 0 { println!("{path}: open fail"); return; }
        let w = wide0(path);
        let mut hc = PDH_HCOUNTER::default();
        let st_e = PdhAddEnglishCounterW(hq, windows::core::PCWSTR(w.as_ptr()), 0, &mut hc);
        let st_l;
        if st_e != 0 {
            let mut hc2 = PDH_HCOUNTER::default();
            st_l = PdhAddCounterW(hq, windows::core::PCWSTR(w.as_ptr()), 0, &mut hc2);
            if st_l == 0 { hc = hc2; }
        } else { st_l = 0; }
        if st_e != 0 && st_l != 0 {
            println!("{path}: english=0x{st_e:08X} local=0x{st_l:08X} -> FAIL");
            let _ = PdhCloseQuery(hq);
            return;
        }
        let _ = PdhCollectQueryData(hq);
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = PdhCollectQueryData(hq);
        let mut size = 0u32; let mut count = 0u32;
        let _ = PdhGetFormattedCounterArrayW(hc, PDH_FMT_DOUBLE, &mut size, &mut count, None);
        let mut out = String::from("ok");
        if size > 0 {
            let mut buf = vec![0usize; size as usize / 8 + 1];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            let s2 = PdhGetFormattedCounterArrayW(hc, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items));
            if s2 == 0 {
                out = format!("ok, instances={count}, first={:?}", if count > 0 { (*items).szName.to_string().ok() } else { None });
            } else { out = format!("array read fail 0x{s2:08X}"); }
        }
        println!("{path}: -> {out}");
        let _ = PdhCloseQuery(hq);
    }
}

fn main() {
    try_add(r"Processor Information(*)% Processor Utility");
    try_add(r"Processor Information(*)% Processor Time");
    try_add(r"Processor(*)% Processor Time");
    try_add(r"PhysicalDisk(*)% Disk Time");
    try_add(r"PhysicalDisk(*)Disk Read Bytes/sec");
    try_add(r"GPU Engine(*)TILIZATION PERCENTAGE");
    try_add(r"GPU Adapter Memory(*)Dedicated Usage");
}
