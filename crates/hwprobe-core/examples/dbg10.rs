#[cfg(target_os = "windows")]
mod win_probe {
// 场景 A:worker 退出后,在别的线程 refresh(复现路径)
// 场景 B:worker 常驻,在别的线程 refresh(对照)
use hwprobe_core::traits::Provider;
use std::sync::Arc;

pub fn main() {
    let keep_alive: Option<std::thread::JoinHandle<()>> = None;
    let (_b, registry) = hwprobe_core::registry::build_on_worker().unwrap();
    println!("worker done; sleep then refresh on new thread");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let r2 = registry.clone();
    let h = std::thread::spawn(move || {
        r2.refresh(None);
        println!("refresh after worker exit: OK");
    });
    h.join().unwrap();
    let _ = keep_alive;
    println!("DONE-A");
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
