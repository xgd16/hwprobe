//! 一次性探针(macOS):显示器 / IMU 数据源实测与取值来源核对。
//!
//! 用途:
//! - 打印每个活动显示的 CoreGraphics / CoreVideo / IOKit(EDID)原始值,
//!   重点核对 `CGDisplayModeGetRefreshRate` 与 CV 标称输出刷新率的差异
//!   (ProMotion 可变刷新率内屏上 CG 返回 0,需要确认哪个来源可靠);
//! - 打印 DisplayProvider / ImuProvider 的实际输出(设备树 + 逐传感器状态),
//!   验证"取不到就标状态、不猜"的降级行为。不是库的一部分。
//!
//! 运行:cargo run -p hwprobe-core --example dbg_mac_display

#[cfg(target_os = "macos")]
fn main() {
    use hwprobe_core::model::SensorKind;
    use hwprobe_core::platform::macos::display::{self, DisplayProvider};
    use hwprobe_core::platform::macos::imu::ImuProvider;
    use hwprobe_core::traits::Provider;

    println!("== 1. 在线显示器原始数据源(CoreGraphics / CoreVideo / IOKit EDID)==");
    let active_ids: Vec<u32> = display::enumerate()
        .map(|l| l.iter().map(|d| d.id).collect())
        .unwrap_or_default();
    match display::enumerate_online() {
        Err(e) => println!("枚举失败: {e}"),
        Ok(list) => {
            for (i, d) in list.iter().enumerate() {
                println!(
                    "[{i}] id={} active={} main={} builtin={} vendor=0x{:04x} model=0x{:04x} serial=0x{:08x}",
                    d.id,
                    active_ids.contains(&d.id),
                    d.is_main,
                    d.is_builtin,
                    d.vendor,
                    d.model,
                    d.serial
                );
                println!(
                    "    像素 {}x{}   逻辑 {}x{}(HiDPI \"UI Looks like\")",
                    d.pixel_width, d.pixel_height, d.width, d.height
                );
                println!(
                    "    CGDisplayModeGetRefreshRate = {:?}   CV 标称输出刷新率 = {:?}   → 采用 {:?}",
                    d.cg_refresh_hz,
                    d.cv_nominal_hz,
                    d.refresh_hz()
                );
                println!(
                    "    全部模式刷新率(面板能力)= {:?}",
                    display::mode_refresh_rates(d.id)
                );
                println!(
                    "    EDID 产品名 = {:?}   IOKit 传输类型 = {:?}   → connector = {:?}",
                    d.edid_name,
                    d.transport,
                    d.connector()
                );
                println!(
                    "    设备 id = {}   名称 = {:?}",
                    d.device_id(),
                    d.display_name(i)
                );
            }
        }
    }

    println!("\n== 2. DisplayProvider 设备树(probe + refresh)==");
    let prov = DisplayProvider::new();
    match prov.probe() {
        Err(e) => println!("probe 失败(类别不可用): {e}"),
        Ok(mut devices) => {
            prov.refresh(&mut devices);
            for (di, dev) in devices.iter().enumerate() {
                println!(
                    "[{di}] {}  id={}  model={}",
                    dev.meta.name, dev.meta.id, dev.meta.model
                );
                for s in &dev.sensors {
                    let kind = match s.desc.kind {
                        SensorKind::Static => "static ",
                        SensorKind::Dynamic => "dynamic",
                    };
                    println!(
                        "      {kind}  {:<14} {:<16} {:<4}  status={:?}  value={:?}",
                        s.desc.code, s.desc.name, s.desc.unit, s.current.status, s.current.value
                    );
                }
            }
        }
    }

    println!("\n== 3. ImuProvider(本机应无 IMU)==");
    let imu = ImuProvider::new();
    match imu.probe() {
        Err(e) => println!("probe 返回 Err(与 Windows 无 IMU 机器行为一致,类别整体不可用):\n  {e}"),
        Ok(devices) => {
            for (di, dev) in devices.iter().enumerate() {
                println!("[{di}] {}  id={}", dev.meta.name, dev.meta.id);
                for s in &dev.sensors {
                    println!(
                        "      {:<16} {:<16} {:<4}  status={:?}  value={:?}",
                        s.desc.code, s.desc.name, s.desc.unit, s.current.status, s.current.value
                    );
                }
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("本探针仅支持 macOS");
}
