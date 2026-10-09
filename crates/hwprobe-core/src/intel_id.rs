//! Intel 处理器 / GPU 代际识别表(平台无关,纯查表)。
//!
//! 数据来源(交叉核对,2026-10):
//! - CPU:CPUID leaf 1 EAX 的 family/model/stepping → 平台代号,对齐
//!   Linux `arch/x86/include/asm/intel-family.h`(torvalds master)
//! - GPU:PCI Device ID → 平台/架构/系列,对齐 Linux `include/drm/intel/pciids.h`
//!   (torvalds master,含 MTL/LNL/ARL/PTL/WCL/NVL/BMG/DG2 全部分组)
//!
//! 原则:同一设备 ID 无法唯一确定的字段(如按 SKU 而非 ID 区分的 EU 数)
//! 留空 / None,绝不编造;未收录的型号返回 None,由调用方跳过展示。

// ---------------- CPU ----------------

/// Intel CPU 识别结果(CPUID family/model/stepping 查表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntelCpuInfo {
    /// 平台代号,如 "Raptor Lake-S"
    pub codename: &'static str,
    /// 核心微架构;混合架构标注 P/E 核。未知留空。
    pub microarch: &'static str,
    /// 市场代际,如 "13th Gen Core"、"Core Ultra Series 2 (200V)"。未知留空。
    pub generation: &'static str,
    /// 制程节点(按计算 tile),如 "Intel 7"。未知留空。
    pub process: &'static str,
}

/// 按 CPUID 签名查 Intel 平台信息。`family`/`model` 为合并扩展位后的
/// 完整值(Intel family 6 时 model = 扩展模型<<4 | 基础模型,
/// 与 raw-cpuid 的 `family_id()`/`model_id()` 返回值一致)。
pub fn cpu_info(family: u8, model: u8, stepping: u8) -> Option<IntelCpuInfo> {
    fn i(codename: &'static str, microarch: &'static str, generation: &'static str, process: &'static str) -> IntelCpuInfo {
        IntelCpuInfo { codename, microarch, generation, process }
    }
    Some(match (family, model) {
        // ---- Core 2 / Nehalem ----
        (6, 0x0F) => i("Merom (Core 2)", "Core", "Core 2", "65nm"),
        (6, 0x17) => i("Penryn (Core 2)", "Core", "Core 2", "45nm"),
        (6, 0x1A) => i("Bloomfield (Nehalem)", "Nehalem", "1st Gen Core", "45nm"),
        (6, 0x1E) => i("Lynnfield / Clarksfield (Nehalem)", "Nehalem", "1st Gen Core", "45nm"),
        (6, 0x25) => i("Arrandale (Westmere)", "Westmere", "1st Gen Core", "32nm"),
        (6, 0x2C) => i("Gulftown (Westmere-EP)", "Westmere", "1st Gen Core (6-core)", "32nm"),
        (6, 0x2E) => i("Nehalem-EX (Xeon 7500)", "Nehalem", "Xeon", "45nm"),
        // ---- Sandy Bridge / Ivy Bridge ----
        (6, 0x2A) => i("Sandy Bridge", "Sandy Bridge", "2nd Gen Core", "32nm"),
        (6, 0x2D) => i("Sandy Bridge-E/EP (Xeon E5 v1)", "Sandy Bridge", "2nd Gen Core / Xeon", "32nm"),
        (6, 0x3A) => i("Ivy Bridge", "Ivy Bridge", "3rd Gen Core", "22nm"),
        (6, 0x3E) => i("Ivy Bridge-E/EP (Xeon E5 v2)", "Ivy Bridge", "3rd Gen Core / Xeon", "22nm"),
        // ---- Haswell / Broadwell ----
        (6, 0x3C) => i("Haswell (Desktop)", "Haswell", "4th Gen Core", "22nm"),
        (6, 0x3F) => i("Haswell-EP (Xeon E5 v3)", "Haswell", "Xeon", "22nm"),
        (6, 0x45) => i("Haswell-ULT/ULX (Mobile)", "Haswell", "4th Gen Core", "22nm"),
        (6, 0x46) => i("Haswell-ULT GT3 (Mobile)", "Haswell", "4th Gen Core", "22nm"),
        (6, 0x3D) => i("Broadwell-U (Mobile)", "Broadwell", "5th Gen Core", "14nm"),
        (6, 0x47) => i("Broadwell-H / Xeon E3 v4", "Broadwell", "5th Gen Core / Xeon", "14nm"),
        (6, 0x4F) => i("Broadwell-EP (Xeon E5 v4)", "Broadwell", "Xeon", "14nm"),
        (6, 0x56) => i("Broadwell-D (Xeon D)", "Broadwell", "Xeon", "14nm"),
        // ---- Skylake 系(14nm 五代同堂,Kaby/Coffee/Comet 均为 Skylake 核心) ----
        (6, 0x4E) => i("Skylake-U/Y (Mobile)", "Skylake", "6th Gen Core", "14nm"),
        (6, 0x5E) => i("Skylake-S/H (Desktop)", "Skylake", "6th Gen Core", "14nm"),
        (6, 0x55) => {
            // 同一 model 以 stepping 区分 Xeon Scalable 三代(intel-family.h)
            let codename = match stepping {
                0..=4 => "Skylake-SP (Xeon Scalable 1st Gen)",
                5..=7 => "Cascade Lake-SP (Xeon Scalable 2nd Gen)",
                _ => "Cooper Lake-SP (Xeon Scalable 3rd Gen)",
            };
            i(codename, "Skylake", "Xeon Scalable", "14nm")
        }
        (6, 0x66) => i("Cannon Lake-U", "Cannon Lake", "8th Gen Core", "10nm"),
        // Kaby Lake-U:0x8E 按 stepping 细分(Amber Lake-Y / Whiskey / Comet Lake-U)
        (6, 0x8E) => {
            let codename = match stepping {
                9 => "Amber Lake-Y",
                10..=11 => "Coffee Lake-U / Whiskey Lake-U",
                12..=15 => "Comet Lake-U (14nm)",
                _ => "Kaby Lake-U",
            };
            let generation = match stepping {
                9 => "7th Gen Core",
                10..=11 => "8th Gen Core",
                12..=15 => "10th Gen Core",
                _ => "7th Gen Core",
            };
            i(codename, "Skylake", generation, "14nm")
        }
        // Kaby Lake-S/H:Coffee Lake(8th/9th Gen)共用 0x9E(stepping >= 10)
        (6, 0x9E) => {
            let (codename, generation) = if stepping >= 10 {
                ("Coffee Lake-S/H", "8th / 9th Gen Core")
            } else {
                ("Kaby Lake-S/H", "7th Gen Core")
            };
            i(codename, "Skylake", generation, "14nm")
        }
        (6, 0xA5) => i("Comet Lake-S", "Skylake", "10th Gen Core", "14nm"),
        (6, 0xA6) => i("Comet Lake-U", "Skylake", "10th Gen Core", "14nm"),
        // ---- 10nm 客户端 ----
        (6, 0x7D) => i("Ice Lake (Client)", "Sunny Cove", "10th Gen Core", "10nm"),
        (6, 0x7E) => i("Ice Lake-U/Y (Mobile)", "Sunny Cove", "10th Gen Core", "10nm"),
        (6, 0x9D) => i("Ice Lake-NNPI", "Sunny Cove", "", "10nm"),
        (6, 0x8C) => i("Tiger Lake-U (Mobile)", "Willow Cove", "11th Gen Core", "10nm SuperFin"),
        (6, 0x8D) => i("Tiger Lake-H (Mobile)", "Willow Cove", "11th Gen Core", "10nm SuperFin"),
        (6, 0xA7) => i("Rocket Lake-S (Desktop)", "Cypress Cove", "11th Gen Core", "14nm"),
        (6, 0x8A) => i("Lakefield", "Sunny Cove (P) + Tremont (E)", "Hybrid (Lakefield)", "10nm"),
        // ---- Alder Lake / Raptor Lake(混合架构) ----
        (6, 0x97) => i("Alder Lake-S", "Golden Cove (P) + Gracemont (E)", "12th Gen Core", "Intel 7"),
        (6, 0x9A) => i("Alder Lake-P/U", "Golden Cove (P) + Gracemont (E)", "12th Gen Core", "Intel 7"),
        (6, 0xBE) => i("Alder Lake-N", "Gracemont (E-core)", "12th Gen Core (N 系列)", "Intel 7"),
        (6, 0xB7) => i("Raptor Lake-S", "Raptor Cove (P) + Gracemont (E)", "13th Gen Core", "Intel 7"),
        (6, 0xBA) => i("Raptor Lake-P/U", "Raptor Cove (P) + Gracemont (E)", "13th Gen Core", "Intel 7"),
        (6, 0xBF) => i("Raptor Lake-S Refresh", "Raptor Cove (P) + Gracemont (E)", "14th Gen Core", "Intel 7"),
        (6, 0xD7) => i("Bartlett Lake", "Raptor Cove (P) + Gracemont (E)", "", "Intel 7"),
        // ---- Core Ultra(Meteor Lake 起) ----
        (6, 0xAC) => i("Meteor Lake-H/P", "Redwood Cove (P) + Crestmont (E)", "Core Ultra Series 1 (100H)", "Intel 4"),
        (6, 0xAA) => i("Meteor Lake-U", "Redwood Cove (P) + Crestmont (E)", "Core Ultra Series 1 (100U)", "Intel 4"),
        (6, 0xBD) => i("Lunar Lake", "Lion Cove (P) + Skymont (E)", "Core Ultra Series 2 (200V)", "TSMC N3B"),
        (6, 0xC5) => i("Arrow Lake-H", "Lion Cove (P) + Skymont (E)", "Core Ultra Series 2 (200H)", "Intel 20A"),
        (6, 0xC6) => i("Arrow Lake-S", "Lion Cove (P) + Skymont (E)", "Core Ultra Series 2 (200S)", "Intel 20A"),
        // Arrow Lake-U 为 Meteor Lake 衍生(Intel 3),品牌归入 Core Ultra 200U
        (6, 0xB5) => i("Arrow Lake-U (Meteor Lake 衍生)", "Redwood Cove+ (P) + Crestmont (E)", "Core Ultra Series 2 (200U)", "Intel 3"),
        (6, 0xCC) => i("Panther Lake", "Cougar Cove (P) + Darkmont (E)", "Core Ultra Series 3 (300)", "Intel 18A"),
        (6, 0xE5) => i("Panther Lake-R", "Cougar Cove (P) + Darkmont (E)", "Core Ultra Series 3 (300, Refresh)", "Intel 18A"),
        (6, 0xD5) => i("Wildcat Lake", "Cougar Cove (P) + Darkmont (E)", "", "Intel 18A"),
        // ---- Xeon 服务器线 ----
        (6, 0x6A) => i("Ice Lake-SP (Xeon Scalable 3rd Gen)", "Sunny Cove", "Xeon Scalable", "10nm"),
        (6, 0x6C) => i("Ice Lake-D (Xeon D-1700/2700)", "Sunny Cove", "Xeon D", "10nm"),
        (6, 0x8F) => i("Sapphire Rapids (Xeon Scalable 4th Gen / Max)", "Golden Cove", "Xeon Scalable", "Intel 7"),
        (6, 0xCF) => i("Emerald Rapids (Xeon Scalable 5th Gen)", "Raptor Cove", "Xeon Scalable", "Intel 7"),
        (6, 0xAD) => i("Granite Rapids-AP (Xeon 6 P-core)", "Redwood Cove", "Xeon 6", "Intel 3"),
        (6, 0xAE) => i("Granite Rapids-D (Xeon 6D)", "Redwood Cove", "Xeon 6", "Intel 3"),
        (19, 0x01) => i("Diamond Rapids (Xeon)", "", "Xeon", ""),
        (18, 0x01) => i("Nova Lake-S", "", "", "Intel 18A"),
        (18, 0x03) => i("Nova Lake-L", "", "", "Intel 18A"),
        // ---- Atom / 低功耗 ----
        (6, 0x5C) => i("Apollo Lake", "Goldmont", "Atom / Pentium / Celeron", "14nm"),
        (6, 0x7A) => i("Gemini Lake", "Goldmont Plus", "Atom / Pentium Silver / Celeron", "14nm"),
        (6, 0x86) => i("Snow Ridge (Tremont)", "Tremont", "Atom (网络/边缘)", "10nm"),
        (6, 0x96) => i("Elkhart Lake", "Tremont", "Atom (工业)", "10nm"),
        (6, 0x9C) => i("Jasper Lake", "Tremont", "Atom / Pentium Silver / Celeron (N50xx/N60xx)", "10nm"),
        (6, 0x1C) | (6, 0x26) => i("Bonnell (Atom)", "Bonnell", "Atom", "45nm"),
        (6, 0x27) | (6, 0x35) => i("Saltwell (Atom)", "Saltwell", "Atom", "32nm"),
        // ---- Xeon Phi ----
        (6, 0x57) => i("Knights Landing", "KNL", "Xeon Phi", "14nm"),
        (6, 0x85) => i("Knights Mill", "KNM", "Xeon Phi", "14nm"),
        // NetBurst(family 15)
        (15, _) => i("NetBurst (Pentium 4 / D)", "NetBurst", "", "90nm~65nm"),
        _ => return None,
    })
}

// ---------------- GPU ----------------

/// Intel GPU(iGPU / dGPU)识别结果(PCI Device ID 查表)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntelGpuInfo {
    /// 所属平台代号,如 "Lunar Lake"、"Alder Lake-S"、"DG2 (Alchemist)"
    pub platform: &'static str,
    /// GPU IP 架构,如 "Xe-LPG"、"Xe2-LPG (Battlemage)"。未知留空。
    pub architecture: &'static str,
    /// 产品系列概称(具体档次以驱动上报名称为准),如 "Arc Graphics 130V/140V"。
    /// 未知留空。
    pub series: &'static str,
    /// 执行单元数:仅在该设备 ID 能唯一对应 EU 配置时给出
    /// (按 SKU 而非 ID 区分的平台留 None)。
    pub execution_units: Option<u32>,
}

struct IntelGpuEntry {
    id: u16,
    info: IntelGpuInfo,
}

macro_rules! entry {
    ($id:expr, $platform:expr, $arch:expr, $series:expr, $eu:expr) => {
        IntelGpuEntry {
            id: $id,
            info: IntelGpuInfo {
                platform: $platform,
                architecture: $arch,
                series: $series,
                execution_units: $eu,
            },
        }
    };
}

/// Intel GPU 表:按平台分组(不要求有序)。设备 ID 分组对齐
/// Linux `include/drm/intel/pciids.h`;EU 数仅在 ID→EU 唯一时填写。
/// 同一平台不同 GT 档次合并表述(如 "UHD Graphics 730/770"),
/// 按设备 ID 能确定档次的(如 ADL-S GT1=UHD 730)写具体值。
const GPU_TABLE: &[IntelGpuEntry] = &[

        // ---- Gen6 Sandy Bridge(GT1 6EU / GT2 12EU) ----
        entry!(0x0102, "Sandy Bridge", "Gen6", "HD Graphics", Some(6)),
        entry!(0x0106, "Sandy Bridge", "Gen6", "HD Graphics", Some(6)),
        entry!(0x010A, "Sandy Bridge", "Gen6", "HD Graphics", Some(6)),
        entry!(0x0112, "Sandy Bridge", "Gen6", "HD Graphics 3000", Some(12)),
        entry!(0x0116, "Sandy Bridge", "Gen6", "HD Graphics 3000", Some(12)),
        entry!(0x0122, "Sandy Bridge", "Gen6", "HD Graphics 3000", Some(12)),
        entry!(0x0126, "Sandy Bridge", "Gen6", "HD Graphics 3000", Some(12)),
        // ---- Gen7 Ivy Bridge(GT1 6EU / GT2 16EU) ----
        entry!(0x0152, "Ivy Bridge", "Gen7", "HD Graphics", Some(6)),
        entry!(0x0156, "Ivy Bridge", "Gen7", "HD Graphics", Some(6)),
        entry!(0x015A, "Ivy Bridge", "Gen7", "HD Graphics", Some(6)),
        entry!(0x0162, "Ivy Bridge", "Gen7", "HD Graphics 4000", Some(16)),
        entry!(0x0166, "Ivy Bridge", "Gen7", "HD Graphics 4000", Some(16)),
        entry!(0x016A, "Ivy Bridge", "Gen7", "HD Graphics 4000", Some(16)),
        // ---- Gen7.5 Haswell(GT1 10 / GT2 20 / GT3 40) ----
        entry!(0x0402, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0406, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x040A, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x040B, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x040E, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0412, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0416, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x041A, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x041B, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x041E, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0422, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0426, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x042A, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x042B, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x042E, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0A02, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0A06, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0A0A, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0A0B, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0A0E, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0A12, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0A16, "Haswell", "Gen7.5", "HD Graphics 4400/4600", Some(20)),
        entry!(0x0A1A, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0A1B, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0A1E, "Haswell", "Gen7.5", "HD Graphics 4200/4400", Some(20)),
        entry!(0x0A22, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0A26, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0A2A, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0A2B, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0A2E, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0C02, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0C06, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0C0A, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0C0B, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0C0E, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0C12, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0C16, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0C1A, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0C1B, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0C1E, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0C22, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0C26, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0C2A, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0C2B, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0C2E, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0D02, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0D06, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0D0A, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0D0B, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0D0E, "Haswell", "Gen7.5", "HD Graphics", Some(10)),
        entry!(0x0D12, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0D16, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0D1A, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0D1B, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0D1E, "Haswell", "Gen7.5", "HD Graphics 4600", Some(20)),
        entry!(0x0D22, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0D26, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0D2A, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0D2B, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        entry!(0x0D2E, "Haswell", "Gen7.5", "Iris / HD 5000", Some(40)),
        // ---- Bay Trail(Atom,Gen7 LP) ----
        entry!(0x0F30, "Bay Trail", "Gen7 LP", "HD Graphics", Some(4)),
        entry!(0x0F31, "Bay Trail", "Gen7 LP", "HD Graphics", Some(4)),
        entry!(0x0F32, "Bay Trail", "Gen7 LP", "HD Graphics", Some(4)),
        entry!(0x0F33, "Bay Trail", "Gen7 LP", "HD Graphics", Some(4)),
        // ---- Ponte Vecchio(数据中心) ----
        entry!(0x0B69, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0B6E, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BD4, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BD5, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BD6, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BD7, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BD8, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BD9, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BDA, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BDB, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BE0, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BE1, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        entry!(0x0BE5, "Ponte Vecchio", "Xe-HPC", "Data Center GPU Max", None),
        // ---- Gen8 Broadwell(GT1 12 / GT2 24 / GT3 48) ----
        entry!(0x1602, "Broadwell", "Gen8", "HD Graphics", Some(12)),
        entry!(0x1606, "Broadwell", "Gen8", "HD Graphics", Some(12)),
        entry!(0x160A, "Broadwell", "Gen8", "HD Graphics", Some(12)),
        entry!(0x160B, "Broadwell", "Gen8", "HD Graphics", Some(12)),
        entry!(0x160D, "Broadwell", "Gen8", "HD Graphics", Some(12)),
        entry!(0x160E, "Broadwell", "Gen8", "HD Graphics", Some(12)),
        entry!(0x1612, "Broadwell", "Gen8", "HD Graphics 6000", Some(24)),
        entry!(0x1616, "Broadwell", "Gen8", "HD Graphics 5500", Some(24)),
        entry!(0x161A, "Broadwell", "Gen8", "HD Graphics P5700", Some(24)),
        entry!(0x161B, "Broadwell", "Gen8", "HD Graphics 5500", Some(24)),
        entry!(0x161D, "Broadwell", "Gen8", "HD Graphics P5700", Some(24)),
        entry!(0x161E, "Broadwell", "Gen8", "HD Graphics 5300", Some(24)),
        entry!(0x1622, "Broadwell", "Gen8", "Iris Pro 6200", Some(48)),
        entry!(0x1626, "Broadwell", "Gen8", "Iris 6100", Some(48)),
        entry!(0x162A, "Broadwell", "Gen8", "Iris Pro P6300", Some(48)),
        entry!(0x162B, "Broadwell", "Gen8", "Iris 6100", Some(48)),
        entry!(0x162D, "Broadwell", "Gen8", "Iris Pro P6300", Some(48)),
        entry!(0x162E, "Broadwell", "Gen8", "Iris 6100", Some(48)),
        entry!(0x1632, "Broadwell", "Gen8", "Iris Pro", Some(48)),
        entry!(0x1636, "Broadwell", "Gen8", "Iris", Some(48)),
        entry!(0x163A, "Broadwell", "Gen8", "Iris Pro", Some(48)),
        entry!(0x163B, "Broadwell", "Gen8", "Iris", Some(48)),
        entry!(0x163D, "Broadwell", "Gen8", "Iris Pro", Some(48)),
        entry!(0x163E, "Broadwell", "Gen8", "Iris", Some(48)),
        // ---- Cherry Trail(Atom,Gen8 LP) ----
        entry!(0x22B0, "Cherry Trail", "Gen8 LP", "HD Graphics", Some(16)),
        entry!(0x22B1, "Cherry Trail", "Gen8 LP", "HD Graphics", Some(16)),
        entry!(0x22B2, "Cherry Trail", "Gen8 LP", "HD Graphics", Some(16)),
        entry!(0x22B3, "Cherry Trail", "Gen8 LP", "HD Graphics", Some(16)),
        // ---- Broxton / Apollo Lake(Gen9 LP) ----
        entry!(0x0A84, "Broxton", "Gen9 LP", "HD Graphics", None),
        entry!(0x1A84, "Broxton", "Gen9 LP", "HD Graphics", None),
        entry!(0x1A85, "Broxton", "Gen9 LP", "HD Graphics", None),
        entry!(0x5A84, "Apollo Lake", "Gen9 LP", "HD Graphics 505", Some(18)),
        entry!(0x5A85, "Apollo Lake", "Gen9 LP", "HD Graphics 500", Some(12)),
        // ---- Gen9 Skylake(GT1 12 / GT1.5 18 / GT2 24 / GT3 48 / GT4 72) ----
        entry!(0x1902, "Skylake", "Gen9", "HD Graphics 510", Some(12)),
        entry!(0x1906, "Skylake", "Gen9", "HD Graphics 510", Some(12)),
        entry!(0x190A, "Skylake", "Gen9", "HD Graphics P510", Some(12)),
        entry!(0x190B, "Skylake", "Gen9", "HD Graphics 510", Some(12)),
        entry!(0x190E, "Skylake", "Gen9", "HD Graphics 515", Some(12)),
        entry!(0x1912, "Skylake", "Gen9", "HD Graphics 530", Some(24)),
        entry!(0x1913, "Skylake", "Gen9", "HD Graphics 510", Some(18)),
        entry!(0x1915, "Skylake", "Gen9", "HD Graphics 515", Some(18)),
        entry!(0x1916, "Skylake", "Gen9", "HD Graphics 520", Some(24)),
        entry!(0x1917, "Skylake", "Gen9", "HD Graphics 512", Some(18)),
        entry!(0x191A, "Skylake", "Gen9", "HD Graphics P530", Some(24)),
        entry!(0x191B, "Skylake", "Gen9", "HD Graphics 530", Some(24)),
        entry!(0x191D, "Skylake", "Gen9", "HD Graphics P530", Some(24)),
        entry!(0x191E, "Skylake", "Gen9", "HD Graphics 515", Some(24)),
        entry!(0x1921, "Skylake", "Gen9", "Iris 540/550", Some(48)),
        entry!(0x1923, "Skylake", "Gen9", "Iris 540", Some(48)),
        entry!(0x1926, "Skylake", "Gen9", "Iris 540", Some(48)),
        entry!(0x1927, "Skylake", "Gen9", "Iris 550", Some(48)),
        entry!(0x192A, "Skylake", "Gen9", "Iris Pro P555", Some(48)),
        entry!(0x192B, "Skylake", "Gen9", "Iris 550", Some(48)),
        entry!(0x192D, "Skylake", "Gen9", "Iris Pro P555", Some(48)),
        entry!(0x1932, "Skylake", "Gen9", "Iris Pro 580", Some(72)),
        entry!(0x193A, "Skylake", "Gen9", "Iris Pro P580", Some(72)),
        entry!(0x193B, "Skylake", "Gen9", "Iris Pro 580", Some(72)),
        entry!(0x193D, "Skylake", "Gen9", "Iris Pro P580", Some(72)),
        // ---- Gemini Lake(Gen9 LP) ----
        entry!(0x3184, "Gemini Lake", "Gen9 LP", "UHD Graphics 600", Some(12)),
        entry!(0x3185, "Gemini Lake", "Gen9 LP", "UHD Graphics 600", Some(12)),
        // ---- Gen9.5 Kaby Lake(同 SKL 档次) ----
        entry!(0x5902, "Kaby Lake", "Gen9.5", "HD Graphics 610", Some(12)),
        entry!(0x5906, "Kaby Lake", "Gen9.5", "HD Graphics 610", Some(12)),
        entry!(0x5908, "Kaby Lake", "Gen9.5", "HD Graphics 610", Some(12)),
        entry!(0x590A, "Kaby Lake", "Gen9.5", "HD Graphics P610", Some(12)),
        entry!(0x590B, "Kaby Lake", "Gen9.5", "HD Graphics 610", Some(12)),
        entry!(0x590E, "Kaby Lake", "Gen9.5", "HD Graphics 615", Some(12)),
        entry!(0x5912, "Kaby Lake", "Gen9.5", "HD Graphics 630", Some(24)),
        entry!(0x5913, "Kaby Lake", "Gen9.5", "HD Graphics 610", Some(18)),
        entry!(0x5915, "Kaby Lake", "Gen9.5", "HD Graphics 615", Some(18)),
        entry!(0x5916, "Kaby Lake", "Gen9.5", "HD Graphics 620", Some(24)),
        entry!(0x5917, "Kaby Lake", "Gen9.5", "Iris Plus 655", Some(48)),
        entry!(0x591A, "Kaby Lake", "Gen9.5", "HD Graphics P630", Some(24)),
        entry!(0x591B, "Kaby Lake", "Gen9.5", "HD Graphics 630", Some(24)),
        entry!(0x591C, "Amber Lake-Y", "Gen9.5", "UHD Graphics 617", Some(24)),
        entry!(0x591D, "Kaby Lake", "Gen9.5", "HD Graphics P630", Some(24)),
        entry!(0x591E, "Kaby Lake", "Gen9.5", "HD Graphics 615", Some(24)),
        entry!(0x5921, "Kaby Lake", "Gen9.5", "HD Graphics 620", Some(24)),
        entry!(0x5923, "Kaby Lake", "Gen9.5", "Iris Plus 640", Some(48)),
        entry!(0x5926, "Kaby Lake", "Gen9.5", "Iris Plus 640/650", Some(48)),
        entry!(0x5927, "Kaby Lake", "Gen9.5", "Iris Plus 650", Some(48)),
        entry!(0x593B, "Kaby Lake", "Gen9.5", "Iris Pro 6400", Some(72)),
        entry!(0x87C0, "Amber Lake-Y", "Gen9.5", "UHD Graphics 617", Some(24)),
        // ---- Gen9.5 Coffee Lake ----
        entry!(0x3E90, "Coffee Lake", "Gen9.5", "UHD Graphics P630", Some(24)),
        entry!(0x3E91, "Coffee Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x3E92, "Coffee Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x3E93, "Coffee Lake", "Gen9.5", "UHD Graphics P630", Some(24)),
        entry!(0x3E94, "Coffee Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x3E96, "Coffee Lake", "Gen9.5", "UHD Graphics P630", Some(24)),
        entry!(0x3E98, "Coffee Lake", "Gen9.5", "UHD Graphics P630", Some(24)),
        entry!(0x3E99, "Coffee Lake", "Gen9.5", "UHD Graphics P630", Some(24)),
        entry!(0x3E9A, "Coffee Lake", "Gen9.5", "UHD Graphics P630", Some(24)),
        entry!(0x3E9B, "Coffee Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x3E9C, "Coffee Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        // ---- Gen9.5 Whiskey Lake ----
        entry!(0x3EA0, "Whiskey Lake", "Gen9.5", "UHD Graphics 620", Some(24)),
        entry!(0x3EA1, "Whiskey Lake", "Gen9.5", "UHD Graphics 605", Some(12)),
        entry!(0x3EA2, "Whiskey Lake", "Gen9.5", "Iris Plus 645", Some(48)),
        entry!(0x3EA3, "Whiskey Lake", "Gen9.5", "UHD Graphics 620", Some(24)),
        entry!(0x3EA4, "Whiskey Lake", "Gen9.5", "UHD Graphics 605", Some(12)),
        // ---- Gen9.5 Coffee Lake-U ----
        entry!(0x3EA5, "Coffee Lake-U", "Gen9.5", "Iris Plus 655", Some(48)),
        entry!(0x3EA6, "Coffee Lake-U", "Gen9.5", "Iris Plus 655", Some(48)),
        entry!(0x3EA7, "Coffee Lake-U", "Gen9.5", "Iris Plus 645/655", Some(48)),
        entry!(0x3EA8, "Coffee Lake-U", "Gen9.5", "Iris Plus 655", Some(48)),
        entry!(0x3EA9, "Coffee Lake-U", "Gen9.5", "UHD Graphics 620", Some(24)),
        entry!(0x87CA, "Amber Lake-Y", "Gen9.5", "UHD Graphics 617", Some(24)),
        // ---- Gen10 Cannon Lake ----
        entry!(0x5A40, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A41, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A42, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A44, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A49, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A4A, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A4C, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A50, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A51, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A52, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A54, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A59, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A5A, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        entry!(0x5A5C, "Cannon Lake", "Gen10", "UHD Graphics 630", None),
        // ---- Gen9.5 Comet Lake ----
        entry!(0x9B21, "Comet Lake-U", "Gen9.5", "UHD Graphics", None),
        entry!(0x9B41, "Comet Lake-U", "Gen9.5", "UHD Graphics 620", Some(24)),
        entry!(0x9BA2, "Comet Lake", "Gen9.5", "UHD Graphics", None),
        entry!(0x9BA4, "Comet Lake", "Gen9.5", "UHD Graphics", None),
        entry!(0x9BA5, "Comet Lake", "Gen9.5", "UHD Graphics", None),
        entry!(0x9BA8, "Comet Lake", "Gen9.5", "UHD Graphics", None),
        entry!(0x9BAA, "Comet Lake", "Gen9.5", "UHD Graphics", None),
        entry!(0x9BAC, "Comet Lake", "Gen9.5", "UHD Graphics", None),
        entry!(0x9BC2, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x9BC4, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x9BC5, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x9BC6, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x9BC8, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x9BCA, "Comet Lake-U", "Gen9.5", "UHD Graphics 620", Some(24)),
        entry!(0x9BCC, "Comet Lake-U", "Gen9.5", "UHD Graphics 620", Some(24)),
        entry!(0x9BE6, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        entry!(0x9BF6, "Comet Lake", "Gen9.5", "UHD Graphics 630", Some(24)),
        // ---- Gen11 Ice Lake(32/48/64 EU 按 SKU 区分,ID 不唯一) ----
        entry!(0x8A50, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A51, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A52, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A53, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A54, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A56, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A57, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A58, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A59, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A5A, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A5B, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A5C, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A5D, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A70, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        entry!(0x8A71, "Ice Lake-U/Y", "Gen11", "Iris Plus / UHD Graphics", None),
        // ---- Elkhart Lake / Jasper Lake(Atom,Gen11 LP) ----
        entry!(0x4541, "Elkhart Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4551, "Elkhart Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4555, "Elkhart Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4557, "Elkhart Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4570, "Elkhart Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4571, "Elkhart Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4E51, "Jasper Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4E55, "Jasper Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4E57, "Jasper Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4E61, "Jasper Lake", "Gen11 LP", "UHD Graphics", None),
        entry!(0x4E71, "Jasper Lake", "Gen11 LP", "UHD Graphics", None),
        // ---- Gen12 Xe-LP Rocket Lake(32EU) ----
        entry!(0x4C80, "Rocket Lake-S", "Xe-LP", "UHD Graphics 750", Some(32)),
        entry!(0x4C8A, "Rocket Lake-S", "Xe-LP", "UHD Graphics 730/750", Some(32)),
        entry!(0x4C8B, "Rocket Lake-S", "Xe-LP", "UHD Graphics 730/750", Some(32)),
        entry!(0x4C8C, "Rocket Lake-S", "Xe-LP", "UHD Graphics 730/750", Some(32)),
        entry!(0x4C90, "Rocket Lake-S", "Xe-LP", "UHD Graphics 750", Some(32)),
        entry!(0x4C9A, "Rocket Lake-S", "Xe-LP", "UHD Graphics 730/750", Some(32)),
        // ---- Gen12 Xe-LP DG1(独显) ----
        entry!(0x4905, "DG1", "Xe-LP", "Iris Xe Max (DG1)", Some(96)),
        entry!(0x4906, "DG1", "Xe-LP", "Iris Xe Max (DG1)", Some(96)),
        entry!(0x4907, "DG1", "Xe-LP", "Iris Xe Max (DG1)", Some(96)),
        entry!(0x4908, "DG1", "Xe-LP", "Iris Xe Max (DG1)", Some(96)),
        entry!(0x4909, "DG1", "Xe-LP", "Iris Xe Max (DG1)", Some(96)),
        // ---- Gen12 Xe-LP Alder Lake-S(GT1=UHD 730 24EU / GT2=UHD 770 32EU) ----
        entry!(0x4680, "Alder Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0x4682, "Alder Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0x4688, "Alder Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0x468A, "Alder Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0x468B, "Alder Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0x4690, "Alder Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0x4692, "Alder Lake-S", "Xe-LP", "UHD Graphics 730", Some(24)),
        entry!(0x4693, "Alder Lake-S", "Xe-LP", "UHD Graphics 730", Some(24)),
        // ---- Gen12 Xe-LP Alder Lake-P(96EU Iris Xe / 64~80EU UHD 按 SKU 区分) ----
        entry!(0x4626, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x4628, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x462A, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46A0, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46A1, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46A2, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46A3, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46A6, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46A8, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46AA, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46B0, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46B1, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46B2, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46B3, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46C0, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46C1, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46C2, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0x46C3, "Alder Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        // ---- Gen12 Xe-LP Alder Lake-N(24/32EU 按 SKU 区分) ----
        entry!(0x46D0, "Alder Lake-N", "Xe-LP", "UHD Graphics", None),
        entry!(0x46D1, "Alder Lake-N", "Xe-LP", "UHD Graphics", None),
        entry!(0x46D2, "Alder Lake-N", "Xe-LP", "UHD Graphics", None),
        entry!(0x46D3, "Alder Lake-N", "Xe-LP", "UHD Graphics", None),
        entry!(0x46D4, "Alder Lake-N", "Xe-LP", "UHD Graphics", None),
        // ---- Gen12 Xe-LP Tiger Lake(GT1 32EU UHD / GT2 96EU Iris Xe) ----
        entry!(0x9A40, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9A49, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9A59, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9A60, "Tiger Lake", "Xe-LP", "UHD Graphics", Some(32)),
        entry!(0x9A68, "Tiger Lake", "Xe-LP", "UHD Graphics", Some(32)),
        entry!(0x9A70, "Tiger Lake", "Xe-LP", "UHD Graphics", Some(32)),
        entry!(0x9A78, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9AC0, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9AC9, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9AD9, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        entry!(0x9AF8, "Tiger Lake", "Xe-LP", "Iris Xe", Some(96)),
        // ---- Xe-LP Raptor Lake-S(GT1=UHD 730 24EU / GT2=UHD 770 32EU) ----
        entry!(0xA780, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA781, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA782, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA783, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA788, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA789, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA78A, "Raptor Lake-S", "Xe-LP", "UHD Graphics 770", Some(32)),
        entry!(0xA78B, "Raptor Lake-S", "Xe-LP", "UHD Graphics 730", Some(24)),
        // ---- Xe-LP Raptor Lake-P/U ----
        entry!(0xA720, "Raptor Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA721, "Raptor Lake-U", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7A0, "Raptor Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7A1, "Raptor Lake-U", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7A8, "Raptor Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7A9, "Raptor Lake-U", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7AA, "Raptor Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7AB, "Raptor Lake-P", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7AC, "Raptor Lake-U", "Xe-LP", "Iris Xe / UHD Graphics", None),
        entry!(0xA7AD, "Raptor Lake-U", "Xe-LP", "Iris Xe / UHD Graphics", None),
        // ---- Xe-LP Lunar Lake(Xe2-LPG,Arc 130V=7 Xe2 / 140V=8 Xe2 按 SKU 区分) ----
        entry!(0x6420, "Lunar Lake", "Xe2-LPG (Battlemage)", "Arc Graphics 130V/140V", None),
        entry!(0x64A0, "Lunar Lake", "Xe2-LPG (Battlemage)", "Arc Graphics 130V/140V", None),
        entry!(0x64B0, "Lunar Lake", "Xe2-LPG (Battlemage)", "Arc Graphics 130V/140V", None),
        // ---- Crescent Island(数据中心,2026) ----
        entry!(0x674C, "Crescent Island", "Xe3P", "Data Center GPU", None),
        entry!(0x674D, "Crescent Island", "Xe3P", "Data Center GPU", None),
        entry!(0x674E, "Crescent Island", "Xe3P", "Data Center GPU", None),
        entry!(0x674F, "Crescent Island", "Xe3P", "Data Center GPU", None),
        entry!(0x6750, "Crescent Island", "Xe3P", "Data Center GPU", None),
        // ---- Xe-LPG Meteor Lake(U 系列 4 Xe = 64EU;H/P 系列按 SKU 7/8 Xe) ----
        entry!(0x7D40, "Meteor Lake-U", "Xe-LPG", "Intel Graphics", Some(64)),
        entry!(0x7D45, "Meteor Lake-U", "Xe-LPG", "Intel Graphics", Some(64)),
        entry!(0x7D55, "Meteor Lake", "Xe-LPG", "Arc Graphics / Intel Graphics", None),
        entry!(0x7D60, "Meteor Lake", "Xe-LPG", "Arc Graphics / Intel Graphics", None),
        entry!(0x7DD5, "Meteor Lake", "Xe-LPG", "Arc Graphics / Intel Graphics", None),
        // ---- Xe-LPG+ Arrow Lake(H 系列 Arc 130T/140T 按 SKU;S/U 系列 4 Xe) ----
        entry!(0x7D41, "Arrow Lake-U", "Xe-LPG+", "Intel Graphics", Some(64)),
        entry!(0x7D51, "Arrow Lake-H", "Xe-LPG+", "Arc Graphics 130T/140T", None),
        entry!(0x7D67, "Arrow Lake-S", "Xe-LPG+", "Intel Graphics (4 Xe)", Some(64)),
        entry!(0x7DD1, "Arrow Lake-H", "Xe-LPG+", "Arc Graphics 130T/140T", None),
        entry!(0xB640, "Arrow Lake-S", "Xe-LPG+", "Intel Graphics (4 Xe)", Some(64)),
        // ---- Xe3-LPG Panther Lake(Arc B330=4 Xe3 / B390=12 Xe3 按 SKU) ----
        entry!(0xB080, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB081, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB082, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB083, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB084, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB085, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB086, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB087, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB08F, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB090, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB0A0, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        entry!(0xB0B0, "Panther Lake", "Xe3-LPG", "Arc Graphics B330/B390", None),
        // ---- Xe3-LPG Wildcat Lake(入门低功耗) ----
        entry!(0xFD80, "Wildcat Lake", "Xe3-LPG", "Intel Graphics", None),
        entry!(0xFD81, "Wildcat Lake", "Xe3-LPG", "Intel Graphics", None),
        // ---- Nova Lake(2026 末发布,S/P 平台) ----
        entry!(0xD740, "Nova Lake-S", "", "", None),
        entry!(0xD741, "Nova Lake-S", "", "", None),
        entry!(0xD742, "Nova Lake-S", "", "", None),
        entry!(0xD743, "Nova Lake-S", "", "", None),
        entry!(0xD745, "Nova Lake-S", "", "", None),
        entry!(0xD74A, "Nova Lake-S", "", "", None),
        entry!(0xD74B, "Nova Lake-S", "", "", None),
        entry!(0xD750, "Nova Lake-P", "", "", None),
        entry!(0xD751, "Nova Lake-P", "", "", None),
        entry!(0xD752, "Nova Lake-P", "", "", None),
        entry!(0xD753, "Nova Lake-P", "", "", None),
        entry!(0xD754, "Nova Lake-P", "", "", None),
        entry!(0xD755, "Nova Lake-P", "", "", None),
        entry!(0xD756, "Nova Lake-P", "", "", None),
        entry!(0xD757, "Nova Lake-P", "", "", None),
        entry!(0xD75F, "Nova Lake-P", "", "", None),
        // ---- Xe-HPG DG2 / Alchemist(Arc A 系列,EU 按具体 SKU) ----
        entry!(0x5690, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5691, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5692, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5693, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5694, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5695, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5696, "DG2-G12 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x5697, "DG2-G12 (Alchemist)", "Xe-HPG", "Arc A-Series (Mobile)", None),
        entry!(0x56A0, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56A1, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56A2, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56A3, "DG2-G12 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56A4, "DG2-G12 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56A5, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56A6, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56B0, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56B1, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56B2, "DG2-G12 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56B3, "DG2-G12 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56BA, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56BB, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56BC, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56BD, "DG2-G11 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56BE, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        entry!(0x56BF, "DG2-G10 (Alchemist)", "Xe-HPG", "Arc A-Series", None),
        // ---- Xe-HPG ATS-M(数据中心 Flex 系列) ----
        entry!(0x56C0, "ATS-M", "Xe-HPG", "Data Center GPU Flex", None),
        entry!(0x56C1, "ATS-M", "Xe-HPG", "Data Center GPU Flex", None),
        entry!(0x56C2, "ATS-M", "Xe-HPG", "Data Center GPU Flex", None),
        // ---- Xe2-HPG Battlemage(Arc B 系列) ----
        entry!(0xE202, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE209, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE20B, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE20C, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE20D, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE210, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE211, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE212, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE216, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE220, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE221, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE222, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
        entry!(0xE223, "Battlemage", "Xe2-HPG", "Arc B-Series", None),
];

/// 按 PCI Device ID 查 Intel GPU 平台信息。表项 ~230 个且仅在 probe
/// 时按设备各查一次,线性查找开销可忽略。
pub fn gpu_info(device_id: u16) -> Option<&'static IntelGpuInfo> {
    GPU_TABLE
        .iter()
        .find(|e| e.id == device_id)
        .map(|e| &e.info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_table_ids_unique() {
        let mut ids: Vec<u16> = GPU_TABLE.iter().map(|e| e.id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "表内有重复设备 ID");
        assert!(total > 200, "表项数量异常: {total}");
    }

    #[test]
    fn cpu_codenames() {
        let cases: &[(u8, u8, &str)] = &[
            (6, 0xA5, "Comet Lake-S"),
            (6, 0x9E, "Kaby Lake-S/H"),     // stepping < 10
            (6, 0xB7, "Raptor Lake-S"),
            (6, 0xBF, "Raptor Lake-S Refresh"),
            (6, 0x97, "Alder Lake-S"),
            (6, 0xAC, "Meteor Lake-H/P"),
            (6, 0xAA, "Meteor Lake-U"),
            (6, 0xBD, "Lunar Lake"),
            (6, 0xC5, "Arrow Lake-H"),
            (6, 0xC6, "Arrow Lake-S"),
            (6, 0xB5, "Arrow Lake-U (Meteor Lake 衍生)"),
            (6, 0xCC, "Panther Lake"),
            (6, 0xD5, "Wildcat Lake"),
            (6, 0xD7, "Bartlett Lake"),
            (18, 0x01, "Nova Lake-S"),
            (19, 0x01, "Diamond Rapids (Xeon)"),
            (15, 0x04, "NetBurst (Pentium 4 / D)"),
        ];
        for &(f, m, want) in cases {
            assert_eq!(cpu_info(f, m, 0).unwrap().codename, want, "family {f:#x} model {m:#x}");
        }
        assert!(cpu_info(6, 0x55, 5).unwrap().codename.contains("Cascade Lake"));
        assert!(cpu_info(6, 0x55, 0).unwrap().codename.contains("Skylake-SP"));
        assert!(cpu_info(6, 0x8E, 12).unwrap().codename.contains("Comet Lake-U"));
        assert!(cpu_info(6, 0x8E, 9).unwrap().codename.contains("Amber Lake"));
        assert!(cpu_info(6, 0x42, 0).is_none(), "未收录型号应返回 None");
    }

    #[test]
    fn gpu_latest_igpu_series() {
        // 最新 Arc 核显系列
        let mtl = gpu_info(0x7D55).unwrap();
        assert_eq!(mtl.platform, "Meteor Lake");
        assert_eq!(mtl.architecture, "Xe-LPG");
        let lnl = gpu_info(0x64A0).unwrap();
        assert_eq!(lnl.architecture, "Xe2-LPG (Battlemage)");
        assert_eq!(lnl.series, "Arc Graphics 130V/140V");
        let arl = gpu_info(0x7D67).unwrap();
        assert_eq!(arl.platform, "Arrow Lake-S");
        assert_eq!(arl.architecture, "Xe-LPG+");
        let ptl = gpu_info(0xB080).unwrap();
        assert_eq!(ptl.platform, "Panther Lake");
        assert_eq!(ptl.architecture, "Xe3-LPG");
        let bmg = gpu_info(0xE202).unwrap();
        assert_eq!(bmg.architecture, "Xe2-HPG");
        assert_eq!(bmg.series, "Arc B-Series");
        // 旧平台 EU 数
        assert_eq!(gpu_info(0x9BC8).unwrap().execution_units, Some(24)); // CML UHD 630
        assert_eq!(gpu_info(0x4693).unwrap().execution_units, Some(24)); // ADL-S UHD 730
        assert_eq!(gpu_info(0x4680).unwrap().execution_units, Some(32)); // ADL-S UHD 770
        assert_eq!(gpu_info(0x9A49).unwrap().execution_units, Some(96)); // TGL Iris Xe
        assert_eq!(gpu_info(0x4693).unwrap().series, "UHD Graphics 730");
        assert!(gpu_info(0x1234).is_none());
    }
}
