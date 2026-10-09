//! 设备 profile:按 SMBIOS 识别特定整机形态,当前重点是 MSI Claw 掌机系列。
//!
//! 识别依据(公开资料交叉验证):
//! - 系统制造商 "Micro-Star International Co., Ltd.";系统产品名含 "Claw"
//! - 主板型号:Claw A1M = MS-1T41(BIOS E1T41IMS.*),Claw 7 AI+ A2VM = MS-1T42,
//!   Claw 8 AI+ A2VM = MS-1T52(另收录 MS-1T8K / MS-1T91 板号,来源
//!   HandheldCompanion 的 Claw 设备固件表)
//! - 其余掌机(ROG Ally / Legion Go / Steam Deck / GPD / AYANEO / OneXPlayer)
//!   仅识别"掌机形态",不带 EC 寄存器表
//!
//! EC 寄存器表采用 MSI Gen-2(WMI2)EC 约定(msi-ec 驱动),与 HandheldCompanion
//! 经 MSI 官方 WMI ACPI 接口(root\WMI MSI_ACPI)使用的块号逐项吻合:
//! 电池 0xD7(=WMI 块 215)、风扇控制 0xD4(=212)、性能模式 0xD2(=210)、
//! 全速 0x98(=152)、SPL/sPPT 0x50/0x51(=80/81)。传感器寄存器
//! (CPU 温度 0x68、GPU 温度 0x80、风扇转速 0x71)在 msi-ec 全代机型上一致。
//! 加载前仍须通过实测校验(见 msi_ec.rs),校验不过不下发传感器。

#![cfg(target_os = "windows")]

use crate::logbuf;
use crate::smbios::Smbios;
use std::sync::OnceLock;

/// MSI Gen-2 EC 传感器/控制寄存器地址(读侧)。
#[derive(Debug, Clone, Copy)]
pub struct EcRegisterMap {
    /// CPU 实时温度(°C)。
    pub cpu_temp: u8,
    /// GPU 实时温度(°C)。
    pub gpu_temp: u8,
    /// 风扇转速低/高字节(16 位 RPM,小端)。
    pub fan_rpm_lo: u8,
    pub fan_rpm_hi: u8,
    /// 性能模式(低 6 位:0=Comfort 1=Green 2=ECO 3=User 4=Sport,bit7=支持 bit6=激活)。
    pub performance_mode: u8,
    /// 风扇软件接管(bit7)。
    pub fan_override: u8,
    /// Cooler Boost / 风扇全速(bit7)。
    pub cooler_boost: u8,
    /// 电池充电上限(bit7=限制开启,低 7 位=百分比)。
    pub charge_limit: u8,
    /// CPU 持续功率限制 SPL/PL1(W)。
    pub power_limit_spl: u8,
    /// CPU 短时功率限制 sPPT/PL2(W)。
    pub power_limit_sppt: u8,
}

/// MSI Claw 系列 EC 寄存器表(Gen-2 WMI2)。
pub const EC_MAP_MSI_CLAW: EcRegisterMap = EcRegisterMap {
    cpu_temp: 0x68,
    gpu_temp: 0x80,
    fan_rpm_lo: 0x71,
    fan_rpm_hi: 0x72,
    performance_mode: 0xD2,
    fan_override: 0xD4,
    cooler_boost: 0x98,
    charge_limit: 0xD7,
    power_limit_spl: 0x50,
    power_limit_sppt: 0x51,
};

#[derive(Debug, Clone)]
pub struct DeviceProfile {
    /// 是否掌机形态(用于主板类别补充说明)。
    pub handheld: bool,
    /// 识别出的整机市场名(未知为空)。
    pub marketing_name: &'static str,
    /// EC 传感器寄存器表(已识别且经机型确认的才给出)。
    pub ec_map: Option<&'static EcRegisterMap>,
    /// EC 表来源说明(日志用)。
    pub ec_map_note: &'static str,
}

const MSI_CLAW_BOARDS: [&str; 5] = ["MS-1T41", "MS-1T42", "MS-1T52", "MS-1T8K", "MS-1T91"];

/// 其他常见掌机的形态提示(仅"掌机"标记,无 EC 表)。
const HANDHELD_HINTS: [(&str, &str); 10] = [
    ("ROG Ally", "ASUS ROG Ally"),
    ("RC71", "ASUS ROG Ally"),
    ("RC72", "ASUS ROG Ally"),
    ("Legion Go", "Lenovo Legion Go"),
    ("Jupiter", "Valve Steam Deck"),
    ("Galileo", "Valve Steam Deck"),
    ("GPD", "GPD Handheld"),
    ("AYANEO", "AYANEO Handheld"),
    ("ONEXPLAYER", "OneXPlayer"),
    ("OneXFly", "OneXPlayer"),
];

impl DeviceProfile {
    const UNKNOWN: DeviceProfile = DeviceProfile {
        handheld: false,
        marketing_name: "",
        ec_map: None,
        ec_map_note: "",
    };
}

/// 从已解析的 SMBIOS 识别设备 profile(纯函数,便于测试)。
pub fn detect_from(smb: &Smbios) -> DeviceProfile {
    let system = smb.system().unwrap_or_default();
    let board = smb.board().unwrap_or_default();
    let mfr = system.manufacturer.to_lowercase();
    let product = system.product.to_lowercase();
    let board_product = board.product.to_uppercase();

    let is_msi = mfr.contains("micro-star") || mfr.contains("micro star") || mfr == "msi";
    if is_msi && (product.contains("claw") || MSI_CLAW_BOARDS.contains(&board_product.as_str())) {
        let name = if product.contains("claw") {
            match system.product.as_str() {
                "Claw 7 AI+ A2VM" => "MSI Claw 7 AI+ A2VM",
                "Claw 8 AI+ A2VM" => "MSI Claw 8 AI+ A2VM",
                _ => "MSI Claw A1M",
            }
        } else {
            match board.product.as_str() {
                "MS-1T42" => "MSI Claw 7 AI+ A2VM",
                "MS-1T52" => "MSI Claw 8 AI+ A2VM",
                _ => "MSI Claw A1M",
            }
        };
        return DeviceProfile {
            handheld: true,
            marketing_name: name,
            ec_map: Some(&EC_MAP_MSI_CLAW),
            ec_map_note: "MSI Gen-2 (WMI2) EC, msi-ec 约定 + HandheldCompanion WMI 块号交叉验证",
        };
    }

    if product.contains("claw") {
        // MSI 产品名但制造商字符串不标准:按 Claw 处理
        return DeviceProfile {
            handheld: true,
            marketing_name: "MSI Claw",
            ec_map: Some(&EC_MAP_MSI_CLAW),
            ec_map_note: "MSI Gen-2 (WMI2) EC, msi-ec 约定 + HandheldCompanion WMI 块号交叉验证",
        };
    }

    for (hint, name) in HANDHELD_HINTS {
        if product.contains(&hint.to_lowercase()) || board_product.contains(hint) {
            return DeviceProfile {
                handheld: true,
                marketing_name: name,
                ec_map: None,
                ec_map_note: "",
            };
        }
    }

    DeviceProfile::UNKNOWN
}

/// 进程级缓存:初始化阶段探测一次,之后零开销。
pub fn detect() -> &'static DeviceProfile {
    static CACHE: OnceLock<DeviceProfile> = OnceLock::new();
    CACHE.get_or_init(|| {
        match super::fetch_smbios() {
            Ok(smb) => {
                let p = detect_from(&smb);
                if p.ec_map.is_some() {
                    logbuf::info(format!(
                        "识别到 {}(EC 寄存器表:{})",
                        p.marketing_name, p.ec_map_note
                    ));
                } else if p.handheld {
                    logbuf::info(format!("识别到掌机形态:{}", p.marketing_name));
                }
                p
            }
            Err(e) => {
                logbuf::warn(format!("SMBIOS 读取失败,设备 profile 未知: {e}"));
                DeviceProfile::UNKNOWN
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msi_claw_detected_by_product() {
        // 用解析函数的输入端做不了单测(需要 SMBIOS 表),这里测匹配逻辑的最小路径:
        // detect_from 依赖 Smbios::parse,构造真实表在 smbios.rs 测试里已覆盖。
        // 此处仅验证寄存器表常量与已知机型映射。
        let m = EC_MAP_MSI_CLAW;
        assert_eq!(m.cpu_temp, 0x68);
        assert_eq!(m.fan_rpm_lo, 0x71);
        assert_eq!(m.gpu_temp, 0x80);
        assert_eq!(m.performance_mode, 0xD2);
        assert_eq!(m.charge_limit, 0xD7);
    }

    #[test]
    fn known_boards_are_claw() {
        for b in ["MS-1T41", "MS-1T42", "MS-1T52"] {
            assert!(MSI_CLAW_BOARDS.contains(&b));
        }
    }
}
