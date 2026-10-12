#[cfg(target_os = "windows")]
mod win_probe {
//! 临时验证:AMD GPU util_gpu_pct 在连续 refresh 下是否持续更新。
//! 用法: cargo run -p hwprobe-core --example dbg_adlx_util

use hwprobe_core::model::{SensorData, Value};
use hwprobe_core::platform::windows::gpu::GpuProvider;
use hwprobe_core::traits::Provider;
use std::time::{Duration, Instant};

fn show(s: Option<&SensorData>) -> String {
    match s {
        Some(s) => format!(
            "{:>6} (status={:?}, ts={})",
            match s.current.value {
                Value::F64(v) => format!("{v:.1}"),
                ref v => format!("{v:?}"),
            },
            s.current.status,
            s.current.timestamp_ms
        ),
        None => "  <无>".into(),
    }
}

pub fn main() {
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let p = GpuProvider::new();
    let mut devs = match p.probe() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("probe 失败: {e}");
            return;
        }
    };
    println!("probe: {} 个 GPU 设备", devs.len());
    for dev in &devs {
        println!("  - {} ({})", dev.meta.name, dev.meta.vendor);
    }

    let t0 = Instant::now();
    for _ in 0..10 {
        p.refresh(&mut devs);
        for dev in &devs {
            let util = dev.sensors.iter().find(|s| s.desc.code == "util_gpu_pct");
            let temp = dev.sensors.iter().find(|s| s.desc.code == "temperature_c");
            println!(
                "t+{:>4}s [{}] util_gpu_pct = {} | temperature_c = {}",
                t0.elapsed().as_secs(),
                dev.meta.name,
                show(util),
                show(temp),
            );
        }
        std::thread::sleep(Duration::from_millis(1000));
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
