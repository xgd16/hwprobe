//! 一次性探针(macOS):电池数据源单位核对 + BatteryProvider 输出验证。
//!
//! 用途:在真机上核对 IOPowerSources / AppleSmartBattery 各键的**原值单位与量级**
//! (电压是不是 mV、温度是不是 0.1K、时间是不是分钟、容量是 mAh 还是百分比),
//! 并对照 provider 的换算结果(mAh→mWh、mA→mW、符号约定)。不是库的一部分。
//!
//! 运行:cargo run -p hwprobe-core --example dbg_mac_battery

#[cfg(target_os = "macos")]
fn main() {
    use hwprobe_core::platform::macos::battery::{
        power_source_snapshot, registry_snapshot, BatteryProvider,
    };
    use hwprobe_core::traits::Provider;

    println!("== IOPowerSources(原值,时间为分钟)==");
    match power_source_snapshot() {
        Some(ps) => println!("{ps:#?}"),
        None => println!("(无电池型电源来源)"),
    }

    println!("\n== AppleSmartBattery 注册表(原值:mV / mA / 0.1K / mAh)==");
    match registry_snapshot() {
        Some(r) => println!("{r:#?}"),
        None => println!("(无电池服务)"),
    }

    let p = BatteryProvider::new();
    let mut devices = match p.probe() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("probe() 失败: {e}");
            std::process::exit(1);
        }
    };
    if devices.is_empty() {
        println!("\nprobe():空清单(本机无电池,台式机属正常情况)");
        return;
    }
    print_tree("probe()", &devices);
    p.refresh(&mut devices);
    print_tree("refresh() 后", &devices);
}

#[cfg(target_os = "macos")]
fn print_tree(title: &str, devices: &[hwprobe_core::model::DeviceData]) {
    use hwprobe_core::model::{SensorKind, Value};

    println!("\n== BatteryProvider {title} ==");
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
                "  [{kind}] {:<24} {:<22} {:<5} = {:<22} {:?}",
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
