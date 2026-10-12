//! 一次性探针(macOS):Network provider 与 CoreWLAN 桥实测。
//!
//! 用途:在本机核对三件事 ——
//!   1. NET_RT_IFLIST2 / getifaddrs 解析出来的接口事实(名、if_index、链路层类型、
//!      MAC、flags、MTU、链路速率、收发字节/错误);
//!   2. `NetProvider` 的 设备 → 传感器 树(probe + 两次 refresh,差分速率要两个采样点);
//!   3. CoreWLAN 桥的原始读数(含噪声、定位服务对 SSID/BSSID 的限制)。
//! 不是库的一部分。
//!
//! 运行:cargo run -p hwprobe-core --example dbg_mac_net

#[cfg(target_os = "macos")]
fn main() {
    use hwprobe_core::model::Value;
    use hwprobe_core::platform::macos::net::{iflist, link_addrs, NetProvider};
    use hwprobe_core::platform::macos::wlan::Wlan;
    use hwprobe_core::traits::Provider;
    use std::time::Duration;

    // ---------- 1. 原始数据源 ----------
    println!("== NET_RT_IFLIST2(sysctl) ==");
    match iflist() {
        Ok(rows) => {
            println!(
                "{:<5} {:<10} {:<7} {:<6} {:>12} {:>14} {:>14} {:>10}",
                "idx", "flags", "link", "mtu", "baud(bps)", "ibytes", "obytes", "ierr/oerr"
            );
            for r in &rows {
                println!(
                    "{:<5} {:<#10x} {:<7} {:<6} {:>12} {:>14} {:>14} {:>10}",
                    r.index,
                    r.flags,
                    if r.link_up() { "up" } else { "down" },
                    r.mtu,
                    r.baudrate,
                    r.ibytes,
                    r.obytes,
                    format!("{}/{}", r.ierrors, r.oerrors)
                );
            }
        }
        Err(e) => println!("  失败: {e}"),
    }

    println!();
    println!("== getifaddrs(AF_LINK)+ MAC 解析 ==");
    let wlan = Wlan::open();
    println!(
        "{:<8} {:<5} {:<6} {:<20} {:<20} {}",
        "name", "idx", "type", "mac(getifaddrs)", "mac(解析后)", "physical?"
    );
    for a in link_addrs() {
        let is_wifi = wlan.as_ref().is_some_and(|w| w.has_interface(&a.name));
        let resolved =
            hwprobe_core::platform::macos::net::resolve_mac(&a.name, &a.mac, is_wifi, wlan.as_ref());
        println!(
            "{:<8} {:<5} {:<6} {:<20} {:<20} {}",
            a.name,
            a.index,
            a.sdl_type,
            if a.mac.is_empty() { "-" } else { &a.mac },
            resolved.as_deref().unwrap_or("(无)"),
            hwprobe_core::platform::macos::net::is_physical(&a.name, a.sdl_type)
        );
    }

    // ---------- 2. provider 树 ----------
    println!();
    println!("== NetProvider(probe + 2×refresh,间隔 1.2s)==");
    let provider = NetProvider::new();
    let mut devices = match provider.probe() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("probe 失败: {e}");
            std::process::exit(1);
        }
    };
    // 差分速率需要两个采样点,首点只建基线
    provider.refresh(&mut devices);
    std::thread::sleep(Duration::from_millis(1200));
    provider.refresh(&mut devices);

    for dev in &devices {
        println!(
            "\n[{}] name={} serial={} (id={})",
            dev.meta.id, dev.meta.name, dev.meta.serial, dev.meta.id
        );
        println!("    {:<18} {:<16} {:>14}  {}", "code", "name", "value", "status");
        for s in &dev.sensors {
            let v = match &s.current.value {
                Value::F64(x) => format!("{x:.1}"),
                Value::U64(x) => x.to_string(),
                Value::I64(x) => x.to_string(),
                Value::Bool(x) => x.to_string(),
                Value::Str(x) => x.clone(),
            };
            let unit = if s.desc.unit.is_empty() {
                String::new()
            } else {
                format!(" {}", s.desc.unit)
            };
            println!(
                "    {:<18} {:<16} {:>14}  {:?}",
                s.desc.code,
                s.desc.name,
                format!("{v}{unit}"),
                s.current.status
            );
        }
    }

    // ---------- 3. CoreWLAN 原始读数 ----------
    println!();
    println!("== CoreWLAN 桥原始读数 ==");
    let Some(wlan) = wlan.as_ref() else {
        println!("  CoreWLAN 不可用(无 WiFi 栈或框架未加载)");
        return;
    };
    let mut found = false;
    for a in link_addrs() {
        if !wlan.has_interface(&a.name) {
            continue;
        }
        found = true;
        println!("\n[{}] (CoreWLAN 站点接口)", a.name);
        println!("    hardwareAddr = {:?}", wlan.hardware_address(&a.name));
        match wlan.query(&a.name) {
            Some(d) => {
                println!("    state        = {}", d.state);
                println!("    connected    = {}", d.connected);
                println!("    radio_on     = {:?}", d.radio_on);
                println!("    ssid         = {:?}", d.ssid);
                println!("    bssid        = {:?}", d.bssid);
                println!("    rssi_dbm     = {:?}", d.rssi_dbm);
                println!("    noise_dbm    = {:?}", d.noise_dbm);
                println!("    tx_mbps      = {:?}", d.tx_mbps);
                println!("    channel      = {:?}", d.channel);
                println!("    band         = {:?}", d.band);
                println!("    freq_mhz     = {:?}", d.freq_mhz());
                println!("    phy          = {:?}", d.phy);
                println!("    security     = {:?}", d.security);
            }
            None => println!("    查询失败"),
        }
    }
    if !found {
        println!("  没有 CoreWLAN 认得的接口(本机可能是无 WiFi 机型或 WiFi 被禁用)");
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("本探针仅支持 macOS");
}
