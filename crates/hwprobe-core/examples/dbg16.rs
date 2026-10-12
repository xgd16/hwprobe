#[cfg(target_os = "windows")]
mod win_probe {
//! SMBus SPD / TSOD 原始寄存器扫描(需管理员)。
//! 用于诊断"为什么没有内存温度":确认 SPD 是否可见、TS 标志位、
//! 0x18-0x1F 有无温度传感器响应。
use hwprobe_core::platform::windows::driver;
use hwprobe_core::platform::windows::smbus;

pub fn main() {
    if let Err(e) = driver::try_init() {
        println!("驱动不可用: {e}");
    }
    let buses = smbus::buses();
    println!("SMBus 总线数 = {}", buses.len());
    for bus in buses {
        println!("== bus {} [{}] ==", bus.index, bus.name);

        // SPD 0x50-0x57:关键字节 + TS 标志 + 页 1 序列号原始字节
        for addr in 0x50u8..=0x57 {
            let key = bus.read_byte_data(addr, 0x02);
            if key.is_none() {
                continue;
            }
            let flag = bus.read_byte_data(addr, 0x0E);
            print!(
                "SPD 0x{addr:02X}: key=0x{:02X?} flag0E=0x{:02X?}",
                key, flag
            );
            if bus.write_byte_data(0x37, 0x00, 0xFF) {
                let mut serial = Vec::new();
                let mut part = Vec::new();
                for i in 0..4u8 {
                    serial.push(bus.read_byte_data(addr, 0x45 + i).unwrap_or(0));
                }
                for i in 0..18u8 {
                    part.push(bus.read_byte_data(addr, 0x49 + i).unwrap_or(0));
                }
                bus.write_byte_data(0x36, 0x00, 0xFF);
                println!(
                    " serial_raw={:02X?} part_raw={:02X?}",
                    serial, part
                );
            } else {
                println!(" (页 1 切换失败)");
            }
        }

        // TSOD 0x18-0x1F:能力寄存器 / 温度寄存器 / Quick Write 探测
        for addr in 0x18u8..=0x1F {
            let cap = bus.read_word_data(addr, 0x00);
            let cfg = bus.read_word_data_swapped(addr, 0x01);
            let temp = bus.read_word_data_swapped(addr, 0x05);
            let quick = bus.quick_write(addr);
            println!(
                "TS  0x{addr:02X}: cap={:?} cfg={:?} temp_raw={:?} quick={}",
                cap, cfg, temp, quick
            );
        }
    }
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
