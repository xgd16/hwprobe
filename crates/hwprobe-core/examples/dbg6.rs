#[cfg(target_os = "windows")]
mod win_probe {
pub fn main() {
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, FIRMWARE_TABLE_PROVIDER};
    unsafe {
        let provider = FIRMWARE_TABLE_PROVIDER(u32::from_be_bytes(*b"RSMB"));
        let size = GetSystemFirmwareTable(provider, 0, None);
        println!("size = {size}");
        if size == 0 { return; }
        let mut buf = vec![0u8; size as usize];
        let written = GetSystemFirmwareTable(provider, 0, Some(&mut buf));
        println!("written = {written}, buf.len = {}", buf.len());
        buf.truncate(written as usize);
        println!("head: {:02X?}", &buf[..16.min(buf.len())]);
        let table_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        println!("table_len = {table_len}");
        let table = &buf[8..];
        match hwprobe_core::smbios::Smbios::parse(table) {
            Ok(sm) => {
                println!("structs = {}", sm.structs.len());
                println!("dimms = {}", sm.dimms().len());
                if let Some(b) = sm.board() { println!("board = {} / {}", b.manufacturer, b.product); }
                if let Some(b) = sm.bios() { println!("bios = {} / {}", b.vendor, b.version); }
            }
            Err(e) => println!("parse err: {e}"),
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
