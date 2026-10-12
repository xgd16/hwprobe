#[cfg(target_os = "windows")]
mod win_probe {
//! 诊断:PDH 索引翻译链
use windows::Win32::System::Performance::*;

fn wide0(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }

fn lookup_index(english: &str) -> Option<u32> {
    let wide = wide0(english);
    let mut idx = 0u32;
    let status = unsafe {
        PdhLookupPerfIndexByNameW(windows::core::PCWSTR::null(), windows::core::PCWSTR(wide.as_ptr()), &mut idx)
    };
    println!("  index({english}) status=0x{status:08X} idx={idx}");
    (status == 0).then_some(idx)
}

fn lookup_name(index: u32) -> Option<String> {
    unsafe {
        let mut chars = 0u32;
        let mut status = PdhLookupPerfNameByIndexW(None, index, None, &mut chars);
        println!("  name(idx={index}) size-query status=0x{status:08X} chars={chars}");
        if chars == 0 { return None; }
        let mut buf = vec![0u16; chars as usize];
        status = PdhLookupPerfNameByIndexW(None, index, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut chars);
        println!("  name(idx={index}) read status=0x{status:08X}");
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..end]);
        println!("  name(idx={index}) = {s:?}");
        Some(s)
    }
}

pub fn main() {
    unsafe {
        println!("== Processor ==");
        let oi = lookup_index("Processor");
        if let Some(i) = oi { lookup_name(i); }
        println!("== % Processor Time ==");
        let ci = lookup_index("% Processor Time");
        if let Some(i) = ci { lookup_name(i); }
        println!("== PhysicalDisk / % Disk Time / GPU Engine ==");
        let a = lookup_index("PhysicalDisk"); let _ = a.and_then(lookup_name);
        let b = lookup_index("% Disk Time"); let _ = b.and_then(lookup_name);
        let c = lookup_index("GPU Engine"); let _ = c.and_then(lookup_name);

        // 组装本地化路径并测试
        if let (Some(o), Some(cn)) = (oi, ci) {
            let o = lookup_name(o).unwrap_or_default();
            let cn = lookup_name(cn).unwrap_or_default();
            let path = format!("{o}(*)\\{cn}");
            println!("localized path = {path:?}");
            let mut hq = PDH_HQUERY::default();
            println!("open=0x{:08X}", PdhOpenQueryW(None, 0, &mut hq));
            let wide = wide0(&path);
            let mut hc = PDH_HCOUNTER::default();
            let st = PdhAddCounterW(hq, windows::core::PCWSTR(wide.as_ptr()), 0, &mut hc);
            println!("add_local=0x{st:08X}");
            if st == 0 {
                println!("collect#1=0x{:08X}", PdhCollectQueryData(hq));
                std::thread::sleep(std::time::Duration::from_millis(1100));
                println!("collect#2=0x{:08X}", PdhCollectQueryData(hq));
                let mut size = 0u32; let mut count = 0u32;
                let s1 = PdhGetFormattedCounterArrayW(hc, PDH_FMT_DOUBLE, &mut size, &mut count, None);
                println!("array size-query=0x{s1:08X} size={size} count={count}");
                if size > 0 {
                    let mut buf = vec![0usize; size as usize / 8 + 1];
                    let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                    let s2 = PdhGetFormattedCounterArrayW(hc, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items));
                    println!("array read=0x{s2:08X} count={count}");
                    for i in 0..count.min(3) as usize {
                        let it = &*items.add(i);
                        println!("  cstatus=0x{:08X} name={:?} val={:?}", it.FmtValue.CStatus, it.szName.to_string(), it.FmtValue.Anonymous.doubleValue);
                    }
                }
            }
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
