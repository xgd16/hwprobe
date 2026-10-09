#![cfg(target_os = "windows")]
use windows::Win32::System::Performance::*;
fn wide0(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }
fn try_add(path: &str) {
    unsafe {
        let mut hq = PDH_HQUERY::default();
        if PdhOpenQueryW(None, 0, &mut hq) != 0 { println!("{path}: open fail"); return; }
        let w = wide0(path);
        let mut hc = PDH_HCOUNTER::default();
        let st = PdhAddCounterW(hq, windows::core::PCWSTR(w.as_ptr()), 0, &mut hc);
        println!("{path}: PdhAddCounter=0x{st:08X}");
        if st == 0 {
            let _ = PdhCollectQueryData(hq);
            let mut v = Default::default();
            let st2 = PdhGetFormattedCounterValue(hc, PDH_FMT_DOUBLE, None, &mut v);
            println!("   collect+value: 0x{st2:08X} val={:?}", v.Anonymous.doubleValue);
        }
        let _ = PdhCloseQuery(hq);
    }
}
fn main() {
    try_add(r"Memory\Available Bytes");
    try_add(r"System\Processor Queue Length");
    try_add(r"Processor(_Total)\% Processor Time");
    try_add(r"Processor Information(_Total)\% Processor Utility");
    try_add(r"PhysicalDisk(0 C:)\% Disk Time");
}
