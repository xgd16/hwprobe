//! 主板 provider:SMBIOS 主板/BIOS/系统识别 + M3 的 SuperIO 风扇电压(驱动层)
//! + 掌机(MSI Claw 等)嵌入式控制器传感器设备(device_profile + msi_ec)。

#![cfg(target_os = "windows")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::platform::windows::{driver, fetch_smbios};
use crate::traits::Provider;

pub struct MotherboardProvider {
    claw_ec: super::msi_ec::ClawEc,
}

impl MotherboardProvider {
    pub fn new() -> MotherboardProvider {
        MotherboardProvider {
            claw_ec: super::msi_ec::ClawEc::new(),
        }
    }
}

impl Provider for MotherboardProvider {
    fn describe(&self) -> &str {
        "win-motherboard"
    }

    fn category(&self) -> Category {
        Category::Motherboard
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let smb = fetch_smbios()?;
        let bios = smb.bios().unwrap_or_default();
        let board = smb.board().unwrap_or_default();
        let system = smb.system().unwrap_or_default();

        let profile = super::device_profile::detect();

        // 设备名:已知整机用市场名补充(如 "MSI Claw A1M")
        let name = if !profile.marketing_name.is_empty() {
            profile.marketing_name.to_string()
        } else if board.product.is_empty() {
            "Motherboard".to_string()
        } else {
            board.product.clone()
        };

        let mut b = DeviceBuilder::new(
            format!("mb:{}", board.serial),
            name,
        )
        .vendor(&board.manufacturer)
        .model(&board.product)
        .serial(&board.serial)
        .static_str("bios_vendor", "BIOS Vendor", &bios.vendor)
        .static_str("bios_version", "BIOS Version", &bios.version)
        .static_str("bios_date", "BIOS Release Date", &bios.release_date)
        .static_str("system_manufacturer", "System Manufacturer", &system.manufacturer)
        .static_str("system_product", "System Product", &system.product);

        if profile.handheld {
            b = b.static_str("form_factor", "Form Factor", "Handheld");
        }

        // MSI WMI 平台风扇(GET_FAN,用户态免驱动;与 EC 传感器互相独立可交叉验证)。
        // 仅在 MSI 机器上尝试,避免非 MSI 机器为找类而全量扫 meta_class。
        // 厂商串兼顾内核 msi-wmi-platform 白名单的两种写法:
        // "Micro-Star International" 与 "MICRO-STAR INT"(空格变体也兼容)。
        let mfr_lower = system.manufacturer.to_lowercase();
        let msi_wmi_info = if mfr_lower.contains("micro-star") || mfr_lower.contains("micro star") {
            super::msi_wmi::probe()
        } else {
            None
        };
        if let Some(info) = msi_wmi_info {
            if !info.ec_firmware.is_empty() {
                b = b.static_str("ec_firmware", "EC Firmware (WMI)", &info.ec_firmware);
            }
            if !info.wmi_version.is_empty() {
                b = b.static_str("msi_wmi_version", "MSI WMI Interface", &info.wmi_version);
            }
            for i in 0..info.fans {
                b = b.dynamic(
                    &format!("fan_wmi_{i}"),
                    &format!("Fan #{i} (WMI)"),
                    "RPM",
                    ValueType::F64,
                );
            }
        }

        // 驱动可用时,SuperIO 探测出的风扇/电压传感器由 refresh 填值;
        // 传感器清单在 probe 时确定(保持键稳定)。
        if driver::is_available() {
            for c in super::driver::plan_sensors() {
                b = b.dynamic(&c.code, &c.name, &c.unit, c.vtype);
            }
        }
        let mut devices = vec![b.build()];

        // 掌机 EC 设备(第二设备;probe 未就绪则跳过,不影响主板本体)
        if let Some(ec_dev) = self.claw_ec.probe() {
            devices.push(ec_dev);
        }

        Ok(devices)
    }

    fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_millis(1500)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        // MSI WMI 风扇:用户态通路,不依赖驱动层。probe 声明过才刷新;
        // 本轮读不到时标 Error(而不是留着 NoDriver 误导)。
        let has_wmi_fans = devices.iter().any(|d| {
            d.sensors
                .iter()
                .any(|s| s.desc.code.starts_with("fan_wmi_"))
        });
        let wmi_fans = if has_wmi_fans {
            super::msi_wmi::refresh_fans()
        } else {
            Vec::new()
        };

        // 驱动层缺失:把动态传感器置为 NO_DRIVER
        if !driver::is_available() {
            for d in devices.iter_mut() {
                for s in d.sensors.iter_mut() {
                    if s.desc.kind == crate::model::SensorKind::Dynamic {
                        s.current = Reading::failed(Status::NoDriver, ValueType::F64);
                    }
                }
            }
        } else {
            super::driver::refresh_motherboard(devices);
        }
        self.claw_ec.refresh(devices);

        if has_wmi_fans {
            for (i, rpm) in wmi_fans.into_iter().enumerate() {
                let code = format!("fan_wmi_{i}");
                for d in devices.iter_mut() {
                    match rpm {
                        Some(v) => d.set_reading(&code, Reading::now(Value::F64(v))),
                        None => {
                            d.set_reading(&code, Reading::failed(Status::Error, ValueType::F64))
                        }
                    }
                }
            }
        }
    }
}
