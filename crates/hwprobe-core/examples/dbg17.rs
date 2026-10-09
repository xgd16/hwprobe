#![cfg(target_os = "windows")]
//! RAPL 能量域扫描(需管理员):确认本机 CPU 暴露哪些功耗域,
//! 特别是是否存在可用于"内存功耗"的 DRAM 域(MSR 0x619),
//! 并用内存负载验证该计数确实随内存活动变化(而非恒定/复用计数)。
//!
//! 用法: dbg17 [--load] [--seconds N]
use hwprobe_core::platform::windows::driver;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MSR_RAPL_POWER_UNIT: u32 = 0x606;
const DOMAINS: [(&str, u32); 5] = [
    ("PKG  (0x611)", 0x611),
    ("PP0  (0x639)", 0x639),
    ("PP1  (0x641)", 0x641),
    ("DRAM (0x619)", 0x619),
    ("PSys (0x64D)", 0x64D),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let with_load = args.iter().any(|a| a == "--load");
    let seconds: f64 = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);

    if let Err(e) = driver::try_init() {
        println!("驱动不可用: {e}");
        return;
    }
    let Some(drv) = driver::access() else {
        println!("驱动不可用");
        return;
    };

    let unit = drv.read_msr(MSR_RAPL_POWER_UNIT);
    let esu = unit.map(|u| ((u >> 8) & 0x1F) as u32).unwrap_or(4);
    let unit_j = 1.0 / (1u64 << esu) as f64;
    println!("MSR 0x606 = {unit:?} (ESU={esu}, 1 计数 = {unit_j} J)");

    for (name, msr) in DOMAINS {
        println!("{name} 能量计数 = {}", fmt(drv.read_msr(msr)));
    }
    println!("DRAM_POWER_INFO (0x61C) = {}", fmt(drv.read_msr(0x61C)));

    if with_load {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        // 每线程独占一块缓冲做读-改-写流式访问,合计 4 GiB,制造内存带宽负载
        const TOTAL: usize = 4 * 1024 * 1024 * 1024;
        const THREADS: usize = 6;
        println!("--- 内存负载启动:{THREADS} 线程 × {} B 缓冲 ---", TOTAL / THREADS);
        for _ in 0..THREADS {
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                let mut slice = vec![0u8; TOTAL / THREADS];
                let mut acc = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let mut i = 0usize;
                    while i + 64 <= slice.len() {
                        // 读-改-写:强制经过内存控制器
                        for k in 0..64 {
                            slice[i + k] = slice[i + k].wrapping_add(1);
                        }
                        acc = acc.wrapping_add(slice[i] as u64);
                        i += 64;
                    }
                }
                acc
            }));
        }
        std::thread::sleep(Duration::from_millis(500));
        sample(drv, unit_j, seconds, "负载中");
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            let _ = h.join();
        }
        sample(drv, unit_j, seconds, "负载后");
    } else {
        sample(drv, unit_j, seconds, "空闲");
    }
}

/// 在 window 秒内首尾各采一次,输出各域平均功率。
fn sample(drv: &'static dyn driver::HwAccess, unit_j: f64, window: f64, tag: &str) {
    let mut before = Vec::new();
    for (name, msr) in DOMAINS {
        before.push((name, msr, drv.read_msr(msr)));
    }
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(window));
    let dt = t0.elapsed().as_secs_f64();
    println!("--- [{tag}] {dt:.3} s 窗口平均功率 ---");
    for (name, msr, v0) in before {
        let v1 = drv.read_msr(msr);
        match (v0, v1) {
            (Some(a), Some(b)) => {
                let d = (b as u32).wrapping_sub(a as u32) as f64;
                if a == 0 && b == 0 {
                    println!("{name}: 恒为 0(该域未实现/被熔断)");
                } else {
                    println!("{name}: Δ={d:>10.0} 计数 → {:>7.2} W", d * unit_j / dt);
                }
            }
            _ => println!("{name}: 读取失败(MSR 不存在或被拒)"),
        }
    }
}

fn fmt(v: Option<u64>) -> String {
    match v {
        Some(x) => format!("0x{x:016X}"),
        None => "读取失败(MSR 不存在或被拒)".to_string(),
    }
}
