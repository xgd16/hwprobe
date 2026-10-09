//! SMBIOS(DMI)表格解析,纯逻辑、可离线单元测试。
//!
//! Windows 侧通过 `GetSystemFirmwareTable('RSMB')` 取得原始表,
//! Linux 侧可直接读 `/sys/firmware/dmi/tables/DMI`(预留)。
//! 解析覆盖:Type 0(BIOS)、Type 1(系统)、Type 2(主板)、Type 17(内存条)。

#[derive(Debug, Clone)]
pub struct SmbiosStruct {
    pub struct_type: u8,
    pub handle: u16,
    pub data: Vec<u8>, // 含 4 字节头
    pub strings: Vec<String>,
}

impl SmbiosStruct {
    /// SMBIOS 字符串引用从 1 开始,0 表示 "未设置"。
    pub fn string(&self, idx: u8) -> Option<&str> {
        if idx == 0 {
            return None;
        }
        self.strings
            .get(idx as usize - 1)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    pub fn u8_at(&self, off: usize) -> Option<u8> {
        self.data.get(off).copied()
    }

    pub fn u16_at(&self, off: usize) -> Option<u16> {
        if off + 2 > self.data.len() {
            return None;
        }
        Some(u16::from_le_bytes([
            self.data[off],
            self.data[off + 1],
        ]))
    }

    pub fn u32_at(&self, off: usize) -> Option<u32> {
        if off + 4 > self.data.len() {
            return None;
        }
        Some(u32::from_le_bytes([
            self.data[off],
            self.data[off + 1],
            self.data[off + 2],
            self.data[off + 3],
        ]))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Smbios {
    pub structs: Vec<SmbiosStruct>,
}

impl Smbios {
    /// 解析裸 SMBIOS 表(不含 RAW_SMBIOS_DATA 头)。格式非法时返回 Err。
    pub fn parse(table: &[u8]) -> Result<Smbios, String> {
        let mut structs = Vec::new();
        let mut pos = 0usize;
        loop {
            if pos + 4 > table.len() {
                break; // 表尾(允许无终止符的截断)
            }
            let struct_type = table[pos];
            let len = table[pos + 1] as usize;
            if len < 4 || pos + len > table.len() {
                // 个别 OEM 结构(type>=128)长度字段不合规:保留已解析的结构,
                // 停止继续解析(不再假设后续结构的重要性)。
                break;
            }
            let handle = u16::from_le_bytes([table[pos + 2], table[pos + 3]]);
            let data = table[pos..pos + len].to_vec();

            // 字符串区:一连串 NUL 结尾字符串,以一个空串结束。
            // 注意:无字符串的结构其字符串区是紧随格式区的双 NUL,必须一次跳过
            // 两个字节,否则会把第二个 NUL 当成下一个结构的类型字节,导致后续
            // 结构全部错位(曾导致 Type 17 内存条结构整段丢失)。
            let mut strings = Vec::new();
            let mut p = pos + len;
            if p + 1 < table.len() && table[p] == 0 && table[p + 1] == 0 {
                p += 2;
            } else {
                loop {
                    let start = p;
                    while p < table.len() && table[p] != 0 {
                        p += 1;
                    }
                    if p >= table.len() {
                        // 表在字符串区中途结束:宽容接受当前结构并停止解析
                        break;
                    }
                    let s = &table[start..p];
                    p += 1; // 跳过字符串结尾 NUL
                    if s.is_empty() {
                        break; // 空串 = 字符串区结束(p 已越过其 NUL)
                    }
                    strings.push(String::from_utf8_lossy(s).trim().to_string());
                }
            }

            let end_marker = struct_type == 127 && len == 4 && strings.is_empty();
            structs.push(SmbiosStruct {
                struct_type,
                handle,
                data,
                strings,
            });
            pos = p;
            if end_marker {
                break;
            }
        }
        if structs.is_empty() {
            return Err("SMBIOS 表为空".into());
        }
        Ok(Smbios { structs })
    }

    pub fn find(&self, struct_type: u8) -> Vec<&SmbiosStruct> {
        self.structs
            .iter()
            .filter(|s| s.struct_type == struct_type)
            .collect()
    }

    // ---------- Type 0: BIOS ----------

    pub fn bios(&self) -> Option<BiosInfo> {
        let found = self.find(0);
        let s = found.first()?;
        Some(BiosInfo {
            vendor: s.string(s.u8_at(0x04).unwrap_or(0)).unwrap_or("").into(),
            version: s.string(s.u8_at(0x05).unwrap_or(0)).unwrap_or("").into(),
            release_date: s.string(s.u8_at(0x08).unwrap_or(0)).unwrap_or("").into(),
        })
    }

    // ---------- Type 1: System ----------

    pub fn system(&self) -> Option<SystemInfo> {
        let found = self.find(1);
        let s = found.first()?;
        Some(SystemInfo {
            manufacturer: s.string(s.u8_at(0x04).unwrap_or(0)).unwrap_or("").into(),
            product: s.string(s.u8_at(0x05).unwrap_or(0)).unwrap_or("").into(),
            version: s.string(s.u8_at(0x06).unwrap_or(0)).unwrap_or("").into(),
            serial: s.string(s.u8_at(0x07).unwrap_or(0)).unwrap_or("").into(),
        })
    }

    // ---------- Type 2: Base Board ----------

    pub fn board(&self) -> Option<BoardInfo> {
        let found = self.find(2);
        let s = found.first()?;
        Some(BoardInfo {
            manufacturer: s.string(s.u8_at(0x04).unwrap_or(0)).unwrap_or("").into(),
            product: s.string(s.u8_at(0x05).unwrap_or(0)).unwrap_or("").into(),
            version: s.string(s.u8_at(0x06).unwrap_or(0)).unwrap_or("").into(),
            serial: s.string(s.u8_at(0x07).unwrap_or(0)).unwrap_or("").into(),
        })
    }

    // ---------- Type 17: Memory Device ----------

    pub fn dimms(&self) -> Vec<DimmInfo> {
        self.find(17)
            .iter()
            .map(|s| {
                let size_raw = s.u16_at(0x0C).unwrap_or(0);
                let size_mb = if size_raw == 0xFFFF {
                    0
                } else if size_raw & 0x8000 != 0 {
                    // SMBIOS 3.2+: bit15 置位表示 KB(用于 <1MB 的小容量)
                    (size_raw & 0x7FFF) as u64 / 1024
                } else if size_raw == 0x7FFF {
                    // >32767MB 走 Extended Size(u32,单位 MB,偏移 0x1C)
                    s.u32_at(0x1C).unwrap_or(0) as u64
                } else {
                    size_raw as u64
                };
                // SMBIOS 3.3:速度 bit15 置位表示 ns(非同步内存),这里统一取低 15 位为 MT/s
                let speed_mts = s.u16_at(0x15).map(|v| (v & 0x7FFF) as u32).unwrap_or(0);
                let cfg_speed = s.u16_at(0x20).map(|v| (v & 0x7FFF) as u32).unwrap_or(0);
                let locator = s
                    .string(s.u8_at(0x10).unwrap_or(0))
                    .unwrap_or("")
                    .to_string();
                let bank = s
                    .string(s.u8_at(0x11).unwrap_or(0))
                    .unwrap_or("")
                    .to_string();
                let mem_type = s.u8_at(0x12).unwrap_or(0);
                DimmInfo {
                    locator: if locator.is_empty() {
                        format!("DIMM{}", s.handle)
                    } else {
                        locator
                    },
                    bank,
                    manufacturer: s
                        .string(s.u8_at(0x17).unwrap_or(0))
                        .unwrap_or("")
                        .to_string(),
                    serial: s.string(s.u8_at(0x18).unwrap_or(0)).unwrap_or("").to_string(),
                    part_number: s
                        .string(s.u8_at(0x1A).unwrap_or(0))
                        .unwrap_or("")
                        .to_string(),
                    size_mb,
                    speed_mts,
                    configured_speed_mts: cfg_speed,
                    form_factor: s.u8_at(0x0E).unwrap_or(0),
                    memory_type: mem_type,
                    memory_type_name: memory_type_name(mem_type).to_string(),
                    rank: s.u8_at(0x1B).map(|a| (a & 0x0F).max(1) as u32),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct BiosInfo {
    pub vendor: String,
    pub version: String,
    pub release_date: String,
}

#[derive(Debug, Clone, Default)]
pub struct SystemInfo {
    pub manufacturer: String,
    pub product: String,
    pub version: String,
    pub serial: String,
}

#[derive(Debug, Clone, Default)]
pub struct BoardInfo {
    pub manufacturer: String,
    pub product: String,
    pub version: String,
    pub serial: String,
}

#[derive(Debug, Clone, Default)]
pub struct DimmInfo {
    pub locator: String,
    pub bank: String,
    pub manufacturer: String,
    pub serial: String,
    pub part_number: String,
    pub size_mb: u64,
    pub speed_mts: u32,
    pub configured_speed_mts: u32,
    pub form_factor: u8,
    pub memory_type: u8,
    pub memory_type_name: String,
    pub rank: Option<u32>,
}

pub fn memory_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "Other",
        0x02 => "Unknown",
        0x03 => "DRAM",
        0x04 => "EDRAM",
        0x05 => "VRAM",
        0x06 => "SRAM",
        0x07 => "RAM",
        0x08 => "ROM",
        0x0C => "Flash",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x16 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM",
        0x25 => "HBM2",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小 Type 17 结构 + 字符串区 + Type 127 终止符。
    fn fixture_dimm() -> Vec<u8> {
        let mut v = Vec::new();
        // Type 17, len 0x24 (2.7+), handle 0x0001
        let mut d = vec![17u8, 0x24, 0x01, 0x00];
        d.resize(0x24, 0);
        d[0x08] = 8; // total width
        d[0x0A] = 8; // data width
        // size = 16384 MB(无 bit15,单位 MB)= 16 GB
        d[0x0C] = 0x00;
        d[0x0D] = 0x40;
        d[0x0E] = 0x0D; // form factor DIMM
        d[0x10] = 1; // locator string 1
        d[0x11] = 2; // bank string 2
        d[0x12] = 0x1A; // DDR4
        // speed = 3200 MT/s(0x0C80)
        d[0x15] = 0x80;
        d[0x16] = 0x0C;
        d[0x17] = 3; // manufacturer string 3
        d[0x18] = 4; // serial string 4
        d[0x1A] = 5; // part number string 5
        d[0x1B] = 0x02; // rank 2
        d[0x20] = 0x80;
        d[0x21] = 0x0C; // configured speed 3200 (0x0C80)
        v.extend_from_slice(&d);
        for s in ["ChannelA-DIMM0", "BANK 0", "Kingston", "SER123", "KHX3200"] {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v.push(0); // 字符串区终止
        // Type 127 终止
        v.extend_from_slice(&[127, 4, 0xFF, 0xFF]);
        v
    }

    #[test]
    fn parse_dimm() {
        let table = fixture_dimm();
        let sm = Smbios::parse(&table).unwrap();
        assert_eq!(sm.structs.len(), 2);
        let dimms = sm.dimms();
        assert_eq!(dimms.len(), 1);
        let d = &dimms[0];
        assert_eq!(d.locator, "ChannelA-DIMM0");
        assert_eq!(d.bank, "BANK 0");
        assert_eq!(d.manufacturer, "Kingston");
        assert_eq!(d.serial, "SER123");
        assert_eq!(d.part_number, "KHX3200");
        // 0x4000 = 16384 MB = 16 GB
        assert_eq!(d.size_mb, 16384);
        assert_eq!(d.memory_type_name, "DDR4");
        assert_eq!(d.speed_mts, 3200);
        assert_eq!(d.configured_speed_mts, 3200);
        assert_eq!(d.rank, Some(2));
    }

    #[test]
    fn parse_bios_board_system() {
        let mut v = Vec::new();
        // Type 0: len 0x18
        let mut d0 = vec![0u8, 0x18, 0x00, 0x00];
        d0.resize(0x18, 0);
        d0[0x04] = 1;
        d0[0x05] = 2;
        d0[0x08] = 3;
        v.extend_from_slice(&d0);
        for s in ["AMI", "F5 BIOS", "01/01/2026"] {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v.push(0);
        // Type 2: len 0x0F
        let mut d2 = vec![2u8, 0x0F, 0x02, 0x00];
        d2.resize(0x0F, 0);
        d2[0x04] = 1;
        d2[0x05] = 2;
        v.extend_from_slice(&d2);
        for s in ["ASUS", "ROG STRIX Z790"] {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v.push(0);
        v.extend_from_slice(&[127, 4, 0xFF, 0xFF]);

        let sm = Smbios::parse(&v).unwrap();
        let bios = sm.bios().unwrap();
        assert_eq!(bios.vendor, "AMI");
        assert_eq!(bios.version, "F5 BIOS");
        let board = sm.board().unwrap();
        assert_eq!(board.manufacturer, "ASUS");
        assert_eq!(board.product, "ROG STRIX Z790");
    }

    #[test]
    fn empty_string_refs() {
        // 字符串索引 0 表示未设置,应返回空
        let mut v = Vec::new();
        let mut d17 = vec![17u8, 0x24, 0x03, 0x00];
        d17.resize(0x24, 0);
        d17[0x0C] = 0x00;
        d17[0x0D] = 0x20; // 0x2000 = 8192 MB
        v.extend_from_slice(&d17);
        v.push(0); // 无字符串
        v.extend_from_slice(&[127, 4, 0xFF, 0xFF]);
        let sm = Smbios::parse(&v).unwrap();
        let dimms = sm.dimms();
        assert_eq!(dimms[0].size_mb, 8192);
        assert!(dimms[0].manufacturer.is_empty());
        assert!(dimms[0].locator.starts_with("DIMM"));
    }

    #[test]
    fn zero_string_struct_does_not_desync() {
        // 回归:无字符串结构的字符串区是双 NUL,必须一次跳两个字节,
        // 否则后续结构错位、Type 17 内存条整体丢失。
        let mut v = Vec::new();
        // Type 3,无字符串
        let mut d3 = vec![3u8, 0x0D, 0x00, 0x00];
        d3.resize(0x0D, 0);
        v.extend_from_slice(&d3);
        v.extend_from_slice(&[0, 0]);
        // Type 8,无字符串
        let mut d8 = vec![8u8, 0x09, 0x01, 0x00];
        d8.resize(0x09, 0);
        v.extend_from_slice(&d8);
        v.extend_from_slice(&[0, 0]);
        // Type 17(有字符串,16 GB DDR4-3200)
        let mut d17 = vec![17u8, 0x24, 0x02, 0x00];
        d17.resize(0x24, 0);
        d17[0x0C] = 0x00;
        d17[0x0D] = 0x40;
        d17[0x0E] = 0x0D;
        d17[0x10] = 1;
        d17[0x12] = 0x1A;
        d17[0x15] = 0x80;
        d17[0x16] = 0x0C;
        d17[0x1A] = 2;
        v.extend_from_slice(&d17);
        for s in ["ChannelA-DIMM0", "ABY4U3000E16161C"] {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v.push(0);
        v.extend_from_slice(&[127, 4, 0xFF, 0xFF]);

        let sm = Smbios::parse(&v).unwrap();
        assert_eq!(sm.structs.len(), 4);
        let dimms = sm.dimms();
        assert_eq!(dimms.len(), 1);
        assert_eq!(dimms[0].part_number, "ABY4U3000E16161C");
        assert_eq!(dimms[0].size_mb, 16384);
        assert_eq!(dimms[0].speed_mts, 3200);
    }

    #[test]
    fn reject_bad_table() {
        assert!(Smbios::parse(&[]).is_err());
        assert!(Smbios::parse(&[17, 0x02, 0, 0]).is_err()); // len < 4
    }
}
