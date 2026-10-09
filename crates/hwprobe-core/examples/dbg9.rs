#![cfg(target_os = "windows")]
use hwprobe_core::traits::Provider;
fn main() {
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
