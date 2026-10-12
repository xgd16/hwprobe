#[cfg(target_os = "windows")]
mod win_probe {
pub fn main() {
    println!("stage: create_backend");
    let mut backend = hwprobe_core::create_backend().unwrap();
    println!("stage: init ({:?})", std::env::var_os("HWPROBE_NO_DRIVER").is_some());
    match backend.init() { Ok(_) => println!("init ok"), Err(e) => println!("init err: {e}") }
    println!("stage: providers");
    let providers = backend.providers();
    println!("providers = {}", providers.len());
    for p in &providers {
        println!("-- probing {}", p.describe());
        match p.probe() {
            Ok(devs) => println!("   ok, {} devices", devs.len()),
            Err(e) => println!("   err: {e}"),
        }
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
