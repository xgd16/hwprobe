#[cfg(target_os = "windows")]
mod win_probe {
use hwprobe_core::traits::Provider;
pub fn main() {
    let mut backend = hwprobe_core::create_backend().unwrap();
    let _ = backend.init();
    for p in backend.providers() {
        println!("-- probe+refresh {}", p.describe());
        let mut devs = match p.probe() { Ok(d) => d, Err(e) => { println!("   probe err: {e}"); continue } };
        println!("   refresh...");
        p.refresh(&mut devs);
        println!("   refresh ok");
    }
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
