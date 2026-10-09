//! 内存 provider:SMBIOS DIMM 识别 + GlobalMemoryStatusEx 动态占用。
//!
//! 可选增强:内存条温度。DDR4 走模块上的 TSOD(DDR4 可选件,SPD 0x0E
//! bit7 标志,SMBus 地址 0x18|槽位,寄存器 0x05);DDR5 走 SPD5118 hub
//! 自带温度(寄存器 0x31)。两者都经 PawnIO SMBus 模块读取,**需要管理员
//! 权限**;驱动/模块不可用或匹配不到内存条时不声明温度传感器(不猜数据)。

#![cfg(target_os = "windows")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::platform::windows::driver;
use crate::platform::windows::smbus::{self, Bus};
use crate::platform::windows::fetch_smbios;
use crate::smbios::DimmInfo;
use crate::traits::Provider;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

// ---------------- SPD / 温度传感器常量 ----------------

/// SPD 的 SMBus 地址范围(DDR4/DDR5 相同,低 3 位对应槽位)。
const SPD_ADDR_MIN: u8 = 0x50;
const SPD_ADDR_MAX: u8 = 0x57;

/// DDR4:SPD 关键字节(0x02)与温度传感器(TSOD)。
const SPD_KEY_BYTE: u8 = 0x02;
const DDR4_KEY: u8 = 0x0C; // DDR4 SDRAM
const DDR4_KEY_E: u8 = 0x0E; // DDR4E SDRAM
const DDR4_TS_FLAG_BYTE: u8 = 0x0E; // bit7 = 模块带温度传感器
const DDR4_TS_FLAG_MASK: u8 = 0x80;
const DDR4_PAGE_SEL: u8 = 0x36; // 页选择伪设备:写 0x36+page,命令 0,值 0xFF
const DDR4_PAGE_MASK: u8 = 0xFF;
const DDR4_TS_ADDR_BASE: u8 = 0x18; // TS 地址 = 0x18 | (spd & 7)
const DDR4_TS_CAP_REG: u8 = 0x00; // 能力寄存器(存在性探测)
const DDR4_TS_TEMP_REG: u8 = 0x05; // 大字端,13 位有符号,0.0625°C/LSB
const DDR4_EEPROM_SERIAL: u16 = 0x145; // 4 字节
const DDR4_EEPROM_PART: u16 = 0x149; // 18 字节
const DDR4_EEPROM_PART_LEN: usize = 18;

/// DDR5:SPD5118 hub 寄存器。
const DDR5_KEY: u8 = 0x12;
const DDR5_DEV_TYPE_MOST: u8 = 0x00; // 期望 0x51
const DDR5_DEV_TYPE_MOST_VAL: u8 = 0x51;
const DDR5_DEV_TYPE_LEAST: u8 = 0x01; // 期望 0x18
const DDR5_DEV_TYPE_LEAST_VAL: u8 = 0x18;
const DDR5_MREG_PAGE: u8 = 0x0B; // MR11 虚拟页
const DDR5_MREG_CAPABILITY: u8 = 0x05; // bit1 = 支持温度传感器
const DDR5_MREG_TS_ENABLE: u8 = 0x1A; // 0 = 温度传感器使能
const DDR5_MREG_TEMP: u8 = 0x31; // 16 位,13 位有符号,0.0625°C/LSB
const DDR5_EEPROM_SERIAL: u16 = 0x205; // 4 字节
const DDR5_EEPROM_PART: u16 = 0x209; // 30 字节
const DDR5_EEPROM_PART_LEN: usize = 30;

// ---------------- 温度源 ----------------

/// 已确认存在的内存条温度传感器。
#[derive(Clone, Copy, Debug)]
enum TempSource {
    /// DDR4 TSOD:独立 SMBus 设备
    Ddr4 { bus: Bus, addr: u8 },
    /// DDR5 SPD5118:温度在 SPD hub 自身的寄存器里
    Ddr5 { bus: Bus, addr: u8 },
}

impl TempSource {
    fn read_celsius(self) -> Option<f64> {
        match self {
            TempSource::Ddr4 { bus, addr } => bus
                .read_word_data_swapped(addr, DDR4_TS_TEMP_REG)
                .map(|raw| tsod_to_celsius(raw, 0x1FFF)),
            TempSource::Ddr5 { bus, addr } => {
                // 温度属于易失数据,须在页 0 读取
                bus.write_byte_data(addr, DDR5_MREG_PAGE, 0);
                bus.read_word_data(addr, DDR5_MREG_TEMP)
                    .map(|raw| tsod_to_celsius(raw, 0x1FFC))
            }
        }
    }
}

/// JEDEC 温度编码:13 位有符号(bit12 为符号位),0.0625°C/LSB。
/// DDR4 TSOD 取低 13 位;DDR5 SPD5118 的 bit1:0 保留,掩 0x1FFC。
fn tsod_to_celsius(raw: u16, mask: u16) -> f64 {
    let v = (raw & mask) as i32;
    let v = if v & 0x1000 != 0 { v - 0x2000 } else { v };
    v as f64 * 0.0625
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpdKind {
    Ddr4,
    Ddr5,
}

/// 一次 SPD 探测的结果。
struct SpdProbe {
    bus: Bus,
    addr: u8,
    kind: SpdKind,
    /// 序列号候选字符串(SMBIOS 侧可能是十进制 LE/BE 或 ASCII,比较时再归一化)
    serial_candidates: Vec<String>,
    part: String,
    temp: Option<TempSource>,
}

// ---------------- SPD 探测 ----------------

fn probe_spd(bus: Bus, addr: u8) -> Option<SpdProbe> {
    // DDR4 要求先选页 0(页选择伪设备 0x36 对 DDR5/空槽是无害的 NACK)
    bus.write_byte_data(DDR4_PAGE_SEL, 0x00, DDR4_PAGE_MASK);
    let key = bus.read_byte_data(addr, SPD_KEY_BYTE)?;
    match key {
        DDR4_KEY | DDR4_KEY_E => probe_ddr4(bus, addr),
        DDR5_KEY => probe_ddr5(bus, addr),
        _ => None,
    }
}

fn probe_ddr4(bus: Bus, addr: u8) -> Option<SpdProbe> {
    // 温度传感器:SPD 标志位为主,标志不可信时探测(Quick Write / 能力寄存器),
    // 与 RAMSPDToolkit 的探测顺序一致。
    let flag = bus.read_byte_data(addr, DDR4_TS_FLAG_BYTE)?;
    let ts_addr = DDR4_TS_ADDR_BASE | (addr & 0x07);
    let temp = if flag & DDR4_TS_FLAG_MASK != 0
        || bus.quick_write(ts_addr)
        || bus.read_word_data(ts_addr, DDR4_TS_CAP_REG).is_some()
    {
        Some(TempSource::Ddr4 { bus, addr: ts_addr })
    } else {
        None
    };

    // 身份信息在页 1(序列号 + 部件号),读完恢复页 0
    let mut serial_bytes = None;
    let mut part = String::new();
    if bus.write_byte_data(DDR4_PAGE_SEL + 1, 0x00, DDR4_PAGE_MASK) {
        serial_bytes = read_bytes_ddr4(bus, addr, DDR4_EEPROM_SERIAL, 4);
        part = read_bytes_ddr4(bus, addr, DDR4_EEPROM_PART, DDR4_EEPROM_PART_LEN)
            .map(|b| ascii_trim(&b))
            .unwrap_or_default();
        bus.write_byte_data(DDR4_PAGE_SEL, 0x00, DDR4_PAGE_MASK);
    }

    Some(SpdProbe {
        bus,
        addr,
        kind: SpdKind::Ddr4,
        serial_candidates: serial_candidates(serial_bytes.as_deref()),
        part,
        temp,
    })
}

fn probe_ddr5(bus: Bus, addr: u8) -> Option<SpdProbe> {
    ensure_ddr5_page0(bus, addr);
    if bus.read_byte_data(addr, DDR5_DEV_TYPE_MOST)? != DDR5_DEV_TYPE_MOST_VAL
        || bus.read_byte_data(addr, DDR5_DEV_TYPE_LEAST)? != DDR5_DEV_TYPE_LEAST_VAL
    {
        return None;
    }
    let temp = match bus.read_byte_data(addr, DDR5_MREG_CAPABILITY) {
        Some(cap) if cap & 0x02 != 0 => {
            let enabled = bus
                .read_byte_data(addr, DDR5_MREG_TS_ENABLE)
                .map(|v| v == 0)
                .unwrap_or(true);
            enabled.then_some(TempSource::Ddr5 { bus, addr })
        }
        _ => None,
    };

    let serial_bytes = read_bytes_ddr5(bus, addr, DDR5_EEPROM_SERIAL, 4);
    let part = read_bytes_ddr5(bus, addr, DDR5_EEPROM_PART, DDR5_EEPROM_PART_LEN)
        .map(|b| ascii_trim(&b))
        .unwrap_or_default();
    bus.write_byte_data(addr, DDR5_MREG_PAGE, 0);

    Some(SpdProbe {
        bus,
        addr,
        kind: SpdKind::Ddr5,
        serial_candidates: serial_candidates(serial_bytes.as_deref()),
        part,
        temp,
    })
}

fn ensure_ddr5_page0(bus: Bus, addr: u8) {
    if let Some(page) = bus.read_byte_data(addr, DDR5_MREG_PAGE) {
        if page & 0x07 != 0 {
            bus.write_byte_data(addr, DDR5_MREG_PAGE, 0);
        }
    }
}

/// DDR4 EEPROM 读(调用前须已切到目标页;本函数用于页 1 内的连续区间)。
fn read_bytes_ddr4(bus: Bus, addr: u8, start: u16, len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    for i in 0..len as u16 {
        out.push(bus.read_byte_data(addr, ((start + i) & 0xFF) as u8)?);
    }
    Some(out)
}

/// DDR5 EEPROM 读:数据区跨页(每 128 字节一页),按页切换后读取。
fn read_bytes_ddr5(bus: Bus, addr: u8, start: u16, len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut cur_page = u8::MAX;
    for i in 0..len as u16 {
        let a = start + i;
        let page = (a >> 7) as u8;
        if page != cur_page {
            if !bus.write_byte_data(addr, DDR5_MREG_PAGE, page) {
                return None;
            }
            cur_page = page;
        }
        out.push(bus.read_byte_data(addr, ((a & 0x7F) as u8) | 0x80)?);
    }
    Some(out)
}

/// 把 SPD 的定长字节区修剪为可见 ASCII(空格/NUL 填充)。
fn ascii_trim(bytes: &[u8]) -> String {
    let s: String = bytes
        .iter()
        .map(|&b| if (0x20..0x7F).contains(&b) { b as char } else { ' ' })
        .collect();
    s.trim().to_string()
}

/// 序列号候选字符串:SMBIOS 侧通常是 4 字节 LE/BE 的十进制,也兼容 ASCII。
fn serial_candidates(bytes: Option<&[u8]>) -> Vec<String> {
    let Some(b) = bytes else {
        return Vec::new();
    };
    let mut v = Vec::new();
    if b.len() == 4 {
        let arr = [b[0], b[1], b[2], b[3]];
        v.push(u32::from_le_bytes(arr).to_string());
        v.push(u32::from_be_bytes(arr).to_string());
        // 也是常见格式:4 字节的十六进制串(BCD 数字序列号在数值上与之一致)
        v.push(format!("{:02X}{:02X}{:02X}{:02X}", arr[0], arr[1], arr[2], arr[3]));
    }
    let ascii = ascii_trim(b);
    if !ascii.is_empty() {
        v.push(ascii);
    }
    v
}

/// 归一化序列号:去空白、去前导零、忽略大小写,便于跨格式比较。
fn normalize_serial(s: &str) -> String {
    let t = s.trim().trim_matches('\0').trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    if t.is_empty() {
        return String::new();
    }
    if t.chars().all(|c| c.is_ascii_hexdigit()) {
        let trimmed = t.trim_start_matches('0');
        let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
        trimmed.to_ascii_uppercase()
    } else {
        t.to_ascii_uppercase()
    }
}

fn serial_matches(smbios: &str, candidates: &[String]) -> bool {
    let a = normalize_serial(smbios);
    !a.is_empty() && candidates.iter().any(|c| normalize_serial(c) == a)
}

fn part_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
}

/// 扫描 SPD,把温度源按索引匹配到 SMBIOS 内存条。
/// 匹配顺序:序列号 → 唯一部件号 → (完全读不到 SPD 身份信息且数量一一对应时)
/// 顺序回退;能读到身份却不匹配时宁可缺温度也不猜。
fn detect_dimm_temps(dimms: &[DimmInfo]) -> Vec<Option<TempSource>> {
    let mut out: Vec<Option<TempSource>> = vec![None; dimms.len()];
    let buses = smbus::buses();
    if buses.is_empty() {
        return out;
    }

    let mut probes = Vec::new();
    for bus in buses {
        for addr in SPD_ADDR_MIN..=SPD_ADDR_MAX {
            if let Some(p) = probe_spd(bus, addr) {
                probes.push(p);
            }
        }
    }
    if probes.is_empty() {
        crate::logbuf::info("SMBus 上未发现 SPD 设备(无内存条或访问被拒)");
        return out;
    }
    for p in &probes {
        crate::logbuf::info(format!(
            "SPD 0x{:02X}@{}: {:?}, part={:?}, serial={:?}, 温度传感器={}",
            p.addr,
            p.bus.name,
            p.kind,
            p.part,
            p.serial_candidates,
            p.temp.is_some()
        ));
    }

    let populated: Vec<usize> = (0..dimms.len()).filter(|&i| dimms[i].size_mb > 0).collect();
    let has_identity = probes
        .iter()
        .any(|p| !p.part.is_empty() || !p.serial_candidates.is_empty());

    let mut assigned = vec![false; dimms.len()];
    let mut leftover: Vec<&SpdProbe> = Vec::new();

    // 1) 序列号
    for p in &probes {
        let hit = populated
            .iter()
            .copied()
            .find(|&i| !assigned[i] && serial_matches(&dimms[i].serial, &p.serial_candidates));
        match hit {
            Some(i) => {
                assigned[i] = true;
                out[i] = p.temp;
                crate::logbuf::info(format!(
                    "SPD 0x{:02X} → {} (序列号匹配, 温度传感器={})",
                    p.addr,
                    dimms[i].locator,
                    p.temp.is_some()
                ));
            }
            None => leftover.push(p),
        }
    }

    // 2) 部件号唯一匹配
    let mut unresolved: Vec<&SpdProbe> = Vec::new();
    for p in leftover {
        let cands: Vec<usize> = populated
            .iter()
            .copied()
            .filter(|&i| !assigned[i] && part_eq(&dimms[i].part_number, &p.part))
            .collect();
        if cands.len() == 1 {
            assigned[cands[0]] = true;
            out[cands[0]] = p.temp;
            crate::logbuf::info(format!(
                "SPD 0x{:02X} → {} (部件号匹配, 温度传感器={})",
                p.addr,
                dimms[cands[0]].locator,
                p.temp.is_some()
            ));
        } else {
            unresolved.push(p);
        }
    }

    // 3) 顺序回退(仅限身份信息完全不可读的情况)
    if !unresolved.is_empty() {
        let free: Vec<usize> = populated.iter().copied().filter(|&i| !assigned[i]).collect();
        if !has_identity && unresolved.len() == free.len() {
            let mut sorted = unresolved.clone();
            sorted.sort_by_key(|p| (p.bus.index, p.addr));
            for (p, i) in sorted.iter().zip(free.iter()) {
                out[*i] = p.temp;
                crate::logbuf::info(format!(
                    "SPD 0x{:02X} 顺序对应 {} (SPD 身份不可读,顺序回退)",
                    p.addr, dimms[*i].locator
                ));
            }
        } else {
            for p in &unresolved {
                crate::logbuf::warn(format!(
                    "SPD 0x{:02X}({}) 无法匹配到 SMBIOS 内存条,其温度不可用",
                    p.addr, p.bus.name
                ));
            }
        }
    }
    out
}

// ---------------- provider ----------------

/// DIMM 温度读取计划(probe 时确定)。
struct DimmPlan {
    device_id: String,
    source: TempSource,
}

pub struct MemoryProvider {
    plan: Mutex<Vec<DimmPlan>>,
    tick: AtomicU64,
}

impl MemoryProvider {
    pub fn new() -> MemoryProvider {
        MemoryProvider {
            plan: Mutex::new(Vec::new()),
            tick: AtomicU64::new(0),
        }
    }
}

impl Provider for MemoryProvider {
    fn describe(&self) -> &str {
        "win-memory"
    }

    fn category(&self) -> Category {
        Category::Memory
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let mut devices = Vec::new();

        // 系统内存设备:动态占用(+ 驱动可用且 CPU 暴露 RAPL DRAM 域时的内存功耗)
        let mut sys = DeviceBuilder::new("memory.system", "Physical Memory")
            .vendor("Microsoft")
            .model("System Memory")
            .dynamic("used_mb", "Memory Used", "MB", ValueType::F64)
            .dynamic("available_mb", "Memory Available", "MB", ValueType::F64)
            .dynamic("used_pct", "Memory Usage", "%", ValueType::F64)
            .dynamic("commit_used_mb", "Commit Charge", "MB", ValueType::F64)
            .dynamic("commit_limit_mb", "Commit Limit", "MB", ValueType::F64);
        if driver::dram_power_supported() {
            sys = sys.dynamic("power_dram", "Memory Power", "W", ValueType::F64);
        }
        devices.push(sys.build());

        // SMBIOS DIMM(+ SMBus 温度探测)
        match fetch_smbios() {
            Ok(smb) => {
                let dimms = smb.dimms();
                let temps = detect_dimm_temps(&dimms);
                let mut plan = Vec::new();
                for (i, d) in dimms.iter().enumerate() {
                    let name = if d.size_mb > 0 {
                        format!("DIMM {}: {} {}", d.locator, d.memory_type_name, d.part_number)
                    } else {
                        format!("DIMM {} (empty)", d.locator)
                    };
                    let id = format!("memory.dimm.{i}:{}", d.serial);
                    let mut b = DeviceBuilder::new(id.clone(), name)
                        .vendor(&d.manufacturer)
                        .model(&d.part_number)
                        .serial(&d.serial)
                        .static_f64("capacity_mb", "Capacity", "MB", d.size_mb as f64);
                    if d.speed_mts > 0 {
                        b = b.static_f64("speed_mts", "Speed", "MT/s", d.speed_mts as f64);
                    }
                    if d.configured_speed_mts > 0 {
                        b = b.static_f64(
                            "configured_speed_mts",
                            "Configured Speed",
                            "MT/s",
                            d.configured_speed_mts as f64,
                        );
                    }
                    b = b.static_str("type", "Type", d.memory_type_name.clone());
                    if let Some(r) = d.rank {
                        b = b.static_f64("rank", "Rank", "", r as f64);
                    }
                    if let Some(src) = temps[i] {
                        b = b.dynamic("temperature_c", "Temperature", "°C", ValueType::F64);
                        plan.push(DimmPlan {
                            device_id: id,
                            source: src,
                        });
                    }
                    devices.push(b.build());
                }
                *self.plan.lock().unwrap_or_else(|e| e.into_inner()) = plan;
            }
            Err(e) => {
                // SMBIOS 拿不到不致命:系统占用数据仍然可用
                crate::logbuf::warn(format!("SMBIOS 读取失败,DIMM 信息缺失: {e}"));
            }
        }
        Ok(devices)
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(1000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        refresh_system_memory(devices);

        // 内存功耗:RAPL DRAM 域(单条 MSR 读,开销可忽略,每拍刷新)
        if driver::is_available() {
            if let Some(dev) = devices.iter_mut().find(|d| d.meta.id == "memory.system") {
                driver::refresh_dram_power(dev);
            }
        }

        // 内存条温度:SMBus 事务较慢(毫秒级且需全局互斥),每 3 拍(约 3 秒)读一次
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        if tick % 3 != 0 {
            return;
        }
        let Ok(plan) = self.plan.lock() else {
            return;
        };
        for p in plan.iter() {
            let Some(dev) = devices.iter_mut().find(|d| d.meta.id == p.device_id) else {
                continue;
            };
            match p.source.read_celsius() {
                Some(t) => dev.set_reading(
                    "temperature_c",
                    Reading::now(Value::F64((t * 10.0).round() / 10.0)),
                ),
                None => dev.set_reading(
                    "temperature_c",
                    Reading::failed(Status::Error, ValueType::F64),
                ),
            }
        }
    }
}

fn refresh_system_memory(devices: &mut [DeviceData]) {    let Some(sys_idx) = devices.iter().position(|d| d.meta.id == "memory.system") else {
        return;
    };
    unsafe {
        let mut ms = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut ms).is_err() {
            devices[sys_idx].set_reading("used_mb", Reading::failed(Status::Error, ValueType::F64));
            devices[sys_idx]
                .set_reading("available_mb", Reading::failed(Status::Error, ValueType::F64));
            return;
        }
        let total = ms.ullTotalPhys as f64;
        let avail = ms.ullAvailPhys as f64;
        let used = total - avail;
        let pct = if total > 0.0 { used / total * 100.0 } else { 0.0 };
        let mb = |v: f64| Value::F64((v / 1024.0 / 1024.0 * 10.0).round() / 10.0);
        devices[sys_idx].set_reading("used_mb", Reading::now(mb(used)));
        devices[sys_idx].set_reading("available_mb", Reading::now(mb(avail)));
        devices[sys_idx].set_reading(
            "used_pct",
            Reading::now(Value::F64((pct * 10.0).round() / 10.0)),
        );
        devices[sys_idx].set_reading(
            "commit_used_mb",
            Reading::now(mb(ms.ullTotalPageFile as f64 - ms.ullAvailPageFile as f64)),
        );
        devices[sys_idx].set_reading(
            "commit_limit_mb",
            Reading::now(mb(ms.ullTotalPageFile as f64)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsod_positive_and_negative() {
        // 13 位有符号,0.0625°C/LSB
        assert_eq!(tsod_to_celsius(0, 0x1FFF), 0.0);
        assert_eq!(tsod_to_celsius(480, 0x1FFF), 30.0); // 0x1E0
        assert_eq!(tsod_to_celsius(0x1FF0, 0x1FFF), -1.0);
        assert_eq!(tsod_to_celsius(0x1F60, 0x1FFF), -10.0);
        // DDR5:bit1:0 保留,不应影响结果
        assert_eq!(tsod_to_celsius(480 | 0x2, 0x1FFC), 30.0);
    }

    #[test]
    fn serial_normalization() {
        assert_eq!(normalize_serial(" 14581666 "), "14581666");
        assert_eq!(normalize_serial("014581666"), "14581666");
        assert_eq!(normalize_serial("0x00AB"), "AB");
        assert_eq!(normalize_serial("abc123"), "ABC123");
        assert!(serial_matches("14581666", &["14581666".into(), "2726491648".into()]));
        assert!(serial_matches("014581666", &["14581666".into()]));
        assert!(!serial_matches("", &["14581666".into()]));
        assert!(!serial_matches("999", &["14581666".into()]));
    }

    #[test]
    fn serial_candidates_le_be() {
        // 14581666 = 0x00DE7FA2 → LE 字节序 A2 7F DE 00
        let c = serial_candidates(Some(&[0xA2, 0x7F, 0xDE, 0x00]));
        assert!(c.contains(&"14581666".to_string()));
        assert!(serial_matches("14581666", &c));
    }

    #[test]
    fn part_number_compare() {
        assert!(part_eq("ABY4U3000E16161C  ", "ABY4U3000E16161C"));
        assert!(part_eq("aby4u3000e16161c", "ABY4U3000E16161C"));
        assert!(!part_eq("", "ABY4U3000E16161C"));
    }

    #[test]
    fn ascii_trim_padding() {
        assert_eq!(ascii_trim(b"ABY4U3000E16161C \0\0"), "ABY4U3000E16161C");
    }
}
