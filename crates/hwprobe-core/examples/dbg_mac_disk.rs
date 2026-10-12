//! 一次性探针(macOS):磁盘数据源核对 + DiskProvider 输出验证。
//!
//! 用途:在真机上核对 IOBlockStorageDevice / IOBlockStorageDriver / IOMedia 各键
//! 的**原值单位与量级**("Total Time" 是不是纳秒、nvme_smart_log 各偏移、
//! SMC NAND 温度键),并对照 provider 的换算与差分速率(首轮不产值)。
//! 不是库的一部分。
//!
//! 运行:cargo run -p hwprobe-core --example dbg_mac_disk

#[cfg(target_os = "macos")]
fn main() {
    use hwprobe_core::platform::macos::disk::{source_snapshots, DiskProvider};
    use hwprobe_core::platform::macos::smc;
    use hwprobe_core::traits::Provider;

    println!("== IOBlockStorageDevice 源(原值:统计时间为纳秒)==");
    let sources = source_snapshots();
    if sources.is_empty() {
        println!("(无带介质的块存储设备)");
    }
    for s in &sources {
        println!("{s:#?}");
    }

    println!("\n== SMC NAND 温度键(TH0x/TH0a/TH0b,单内置盘机器才归属到该盘)==");
    for key in ["TH0x", "TH0a", "TH0b"] {
        match smc::read_f64(key) {
            Some(v) => println!("  {key} = {v:.1} °C"),
            None => println!("  {key} = <读不到>"),
        }
    }

    let p = DiskProvider::new();
    let mut devices = match p.probe() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("probe() 失败: {e}");
            std::process::exit(1);
        }
    };
    print_tree("probe()", &devices);

    // 首轮 refresh 只建差分基线:吞吐/活动率应保持 Waiting
    std::thread::sleep(std::time::Duration::from_millis(1200));
    p.refresh(&mut devices);
    print_tree("refresh() #1(首轮基线,吞吐应 Waiting)", &devices);

    // 制造一点磁盘 I/O,验证差分速率不是常 0
    {
        use std::io::Write;
        let path = std::env::temp_dir().join("dbg_mac_disk_io.tmp");
        if let Ok(mut f) = std::fs::File::create(&path) {
            let block = vec![0u8; 1024 * 1024];
            for _ in 0..16 {
                let _ = f.write_all(&block);
            }
            let _ = f.sync_all();
        }
        let _ = std::fs::remove_file(&path);
    }

    std::thread::sleep(std::time::Duration::from_millis(1200));
    p.refresh(&mut devices);
    print_tree("refresh() #2(差分速率)", &devices);
}

#[cfg(target_os = "macos")]
fn print_tree(title: &str, devices: &[hwprobe_core::model::DeviceData]) {
    use hwprobe_core::model::{SensorKind, Value};

    println!("\n== DiskProvider {title} ==");
    for d in devices {
        println!(
            "device {} | name={} model={} vendor={} serial={}",
            d.meta.id, d.meta.name, d.meta.model, d.meta.vendor, d.meta.serial
        );
        for s in &d.sensors {
            let kind = match s.desc.kind {
                SensorKind::Static => "static",
                SensorKind::Dynamic => "dyn   ",
            };
            let val = match &s.current.value {
                Value::F64(v) => format!("{v}"),
                Value::U64(v) => format!("{v}"),
                Value::I64(v) => format!("{v}"),
                Value::Bool(v) => format!("{v}"),
                Value::Str(v) => format!("{v:?}"),
            };
            println!(
                "  [{kind}] {:<14} {:<18} {:<5} = {:<48} {:?}",
                s.desc.code,
                s.desc.name,
                s.desc.unit,
                val,
                s.current.status
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("本探针仅支持 macOS");
}
