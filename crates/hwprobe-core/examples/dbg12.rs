#![cfg(target_os = "windows")]
//! 0x8637 芯片 BAR 空间寄存器读取(基址 0x730)
use hwprobe_core::platform::windows::driver;

fn main() {
    let Ok(()) = driver::try_init() else { println!("驱动失败"); return };
    let Some(drv) = driver::access() else { println!("无驱动"); return };
    if !drv.superio_begin(0) { println!("begin 失败"); return }
    // 进入配置态(ITE 序列)
    for b in [0x87u8, 0x01, 0x55, 0x55] { drv.write_io_port_byte(0x2E, b); }
    // 读芯片 ID 与版本
    let mut cr = |reg: u8| -> u8 { drv.write_io_port_byte(0x2E, reg); drv.read_io_port_byte(0x2F).unwrap_or(0xFF) };
    println!("CR20={:02X} CR21={:02X} CR22={:02X}", cr(0x20), cr(0x21), cr(0x22));
    // 选 LDN4,读基址
    drv.write_io_port_byte(0x2E, 0x07); drv.write_io_port_byte(0x2F, 0x04);
    let base = ((cr(0x60) as u16) << 8) | cr(0x61) as u16;
    println!("LDN4 base = {base:04X}");
    let bars_ok = drv.superio_scan_bars();
    println!("find_bars = {bars_ok}");
    // 退出配置态
    drv.write_io_port_byte(0x2E, 0x02); drv.write_io_port_byte(0x2F, 0x02);

    // BAR 索引读取(BAR+5 索引 / BAR+6 数据)
    let bar_rd = |reg: u8| -> Option<u8> { drv.write_io_port_byte(base + 5, reg); drv.read_io_port_byte(base + 6) };
    print!("BAR: ");
    for reg in [0x00u8, 0x0C, 0x0D, 0x0E, 0x0F, 0x18, 0x19, 0x1A, 0x20, 0x21, 0x22, 0x29, 0x2A, 0x2B, 0x58] {
        match bar_rd(reg) { Some(v) => print!("{reg:02X}={v:02X} "), None => print!("{reg:02X}=?? ") }
    }
    println!();
    println!("DONE");
}
