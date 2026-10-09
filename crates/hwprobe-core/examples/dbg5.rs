#![cfg(target_os = "windows")]
use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::*;
fn main() {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
        let mut i = 0u32;
        while let Ok(a) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = a.GetDesc1() else { continue };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 { continue; }
            let name = String::from_utf16_lossy(&desc.Description[..desc.Description.iter().position(|&c| c==0).unwrap_or(0)]);
            println!("adapter {}: {} dedicated={}MB", i-1, name, desc.DedicatedVideoMemory/1024/1024);
            let a3: IDXGIAdapter3 = a.cast().unwrap();
            for seg in [DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL] {
                let mut info = Default::default();
                let st = a3.QueryVideoMemoryInfo(0, seg, &mut info);
                println!("  seg={seg:?} ok={} budget={}MB used={}MB", st.is_ok(), info.Budget/1024/1024, info.CurrentUsage/1024/1024);
            }
        }
    }
}
