#[cfg(target_os = "windows")]
mod win_probe {
use std::sync::Arc;
pub fn main() {
    println!("stage: build_on_worker");
    let (_backend, registry) = hwprobe_core::registry::build_on_worker().unwrap();
    println!("stage: spawn_poller");
    let r: Arc<hwprobe_core::Registry> = registry.into();
    r.spawn_poller();
    println!("stage: sleep");
    std::thread::sleep(std::time::Duration::from_millis(2500));
    println!("stage: device_count CPU = {:?}", r.device_count(0));
    println!("stage: refresh all");
    r.refresh(None);
    println!("ALL DONE");
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
