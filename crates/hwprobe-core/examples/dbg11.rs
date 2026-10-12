#[cfg(target_os = "windows")]
mod win_probe {
//! SuperIO 芯片探测诊断:在两个配置槽位上尝试各厂商进入序列并读芯片 ID。
use hwprobe_core::platform::windows::driver;

fn rd(drv: &dyn driver::HwAccess, reg_port: u16, val_port: u16, reg: u8) -> u8 {
    drv.write_io_port_byte(reg_port, reg);
    drv.read_io_port_byte(val_port).unwrap_or(0xFF)
}

fn wr(drv: &dyn driver::HwAccess, port: u16, val: u8) {
    drv.write_io_port_byte(port, val);
}

fn dump_crs(drv: &dyn driver::HwAccess, reg_port: u16, val_port: u16, tag: &str) {
    let regs = [0x20u8, 0x21, 0x1F, 0x4F, 0x58, 0x60, 0x61, 0x62, 0x63];
    let mut s = String::new();
    for r in regs {
        s.push_str(&format!(" {r:02X}={:02X}", rd(drv, reg_port, val_port, r)));
    }
    println!("    [{tag}]{s}");
}

pub fn main() {
    match driver::try_init() {
        Ok(()) => println!("驱动初始化 OK"),
        Err(e) => {
            println!("驱动初始化失败: {e}");
            return;
        }
    }
    let Some(drv) = driver::access() else {
        println!("无驱动访问接口");
        return;
    };

    for &(slot, reg_port, val_port) in &[(0u8, 0x2Eu16, 0x2Fu16), (1u8, 0x4Eu16, 0x4Fu16)] {
        println!("==== 槽位 {slot}({reg_port:02X}/{val_port:02X})");
        if !drv.superio_begin(slot) {
            println!("  superio_begin 失败");
            continue;
        }

        // 先直接读(可能已在配置态)
        dump_crs(drv, reg_port, val_port, "直接读");

        // ITE 序列
        wr(drv, reg_port, 0x87);
        wr(drv, reg_port, 0x01);
        wr(drv, reg_port, 0x55);
        wr(drv, reg_port, if reg_port == 0x4E { 0xAA } else { 0x55 });
        dump_crs(drv, reg_port, val_port, "ITE序列");

        // Nuvoton/Winbond 序列
        wr(drv, reg_port, 0x87);
        wr(drv, reg_port, 0x87);
        dump_crs(drv, reg_port, val_port, "Nuvoton");

        // SMSC 序列
        wr(drv, reg_port, 0x55);
        dump_crs(drv, reg_port, val_port, "SMSC");

        // PawnIO:find_bars + LDN4 基址
        if drv.superio_scan_bars() {
            println!("  find_bars OK");
        }
        wr(drv, reg_port, 0x07);
        wr(drv, val_port, 0x04);
        let hi = rd(drv, reg_port, val_port, 0x60) as u16;
        let lo = rd(drv, reg_port, val_port, 0x61) as u16;
        println!("  LDN4 基址读回: {hi:02X}{lo:02X}");
        // 扫描 LDN 0..0x10 的 0x20 寄存器(设备存在性)
        let mut present = Vec::new();
        for ldn in 0u8..0x10 {
            wr(drv, reg_port, 0x07);
            wr(drv, val_port, ldn);
            let id = rd(drv, reg_port, val_port, 0x20);
            if id != 0 && id != 0xFF {
                present.push(format!("LDN{ldn:02X}:20={id:02X}"));
            }
        }
        println!("  非空 LDN: {}", present.join(", "));

        // 退出配置态
        wr(drv, reg_port, 0x02);
        wr(drv, val_port, 0x02);
    }
    println!("DONE");
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
