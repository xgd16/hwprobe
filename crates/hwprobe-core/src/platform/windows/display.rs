//! Display(显示器/面板)provider:DisplayConfig 查询活动路径的分辨率、
//! 刷新率与输出技术,Raw Input 枚举触屏数字化器。
//! 掌机内屏(如 MSI Claw 的 7" 1080p 120Hz)由此暴露。全部用户态接口。

#![cfg(target_os = "windows")]

use crate::model::{Category, DeviceBuilder, DeviceData, Reading, Status, Value, ValueType};
use crate::traits::Provider;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
    QDC_ONLY_ACTIVE_PATHS,
};

pub struct DisplayProvider;

impl DisplayProvider {
    pub fn new() -> DisplayProvider {
        DisplayProvider
    }
}

fn tech_name(t: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> &'static str {
    match t.0 {
        -1 => "Other",
        0 => "VGA (HD15)",
        1 => "S-Video",
        2 => "Composite",
        3 => "Component",
        4 => "DVI",
        5 => "HDMI",
        6 => "LVDS (embedded)",
        8 => "D-Jpn",
        9 => "SDI",
        10 => "DisplayPort (external)",
        11 => "DisplayPort (embedded)",
        12 => "UDI (external)",
        13 => "UDI (embedded)",
        14 => "SDTV dongle",
        15 => "Miracast",
        16 => "Indirect wired",
        17 => "Indirect virtual",
        18 => "DisplayPort USB tunnel",
        _ => "Unknown",
    }
}

fn wide_str(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// 活动显示路径(分辨率/刷新率/输出技术)。
struct ActiveDisplay {
    adapter_id: u64,
    target_id: u32,
    friendly_name: String,
    tech: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
    width: u32,
    height: u32,
    refresh_hz: Option<f64>,
}

fn enumerate_active() -> Result<Vec<ActiveDisplay>, String> {
    unsafe {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let err = GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count);
        if err.is_err() || path_count == 0 {
            return Err(format!("GetDisplayConfigBufferSizes 失败: {err:?}"));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let err = QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        );
        if err.is_err() {
            return Err(format!("QueryDisplayConfig 失败: {err:?}"));
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);

        let mut out = Vec::new();
        for p in &paths {
            let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            name.header.adapterId = p.targetInfo.adapterId;
            name.header.id = p.targetInfo.id;
            let hr = DisplayConfigGetDeviceInfo(&mut name.header);
            if hr != 0 {
                continue;
            }
            // 目标模式(分辨率)从 modeInfo 里按 (adapterId, id) 匹配
            let target_mode = modes.iter().find(|m| {
                m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
                    && m.adapterId.LowPart == p.targetInfo.adapterId.LowPart
                    && m.adapterId.HighPart == p.targetInfo.adapterId.HighPart
                    && m.id == p.targetInfo.id
            });
            let (w, h, refresh) = match target_mode {
                Some(m) => {
                    let v = &m.Anonymous.targetMode.targetVideoSignalInfo;
                    let refresh = if v.vSyncFreq.Denominator > 0 {
                        Some(v.vSyncFreq.Numerator as f64 / v.vSyncFreq.Denominator as f64)
                    } else if p.targetInfo.refreshRate.Denominator > 0 {
                        Some(
                            p.targetInfo.refreshRate.Numerator as f64
                                / p.targetInfo.refreshRate.Denominator as f64,
                        )
                    } else {
                        None
                    };
                    (v.activeSize.cx, v.activeSize.cy, refresh)
                }
                None => (0, 0, None),
            };
            out.push(ActiveDisplay {
                adapter_id: ((p.targetInfo.adapterId.HighPart as u64) << 32)
                    | p.targetInfo.adapterId.LowPart as u64,
                target_id: p.targetInfo.id,
                friendly_name: wide_str(&name.monitorFriendlyDeviceName),
                tech: name.outputTechnology,
                width: w,
                height: h,
                refresh_hz: refresh,
            });
        }
        Ok(out)
    }
}

/// 触屏数字化器数量(Raw Input,HID UsagePage 0x0D / Usage 0x04 TouchScreen)。
fn count_touchscreens() -> u32 {
    use windows::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RIDI_DEVICEINFO, RAWINPUTDEVICELIST,
        RID_DEVICE_INFO,
    };

    unsafe {
        let mut n = 0u32;
        if GetRawInputDeviceList(None, &mut n, std::mem::size_of::<RAWINPUTDEVICELIST>() as u32) != 0
        {
            return 0;
        }
        if n == 0 {
            return 0;
        }
        let mut list = vec![RAWINPUTDEVICELIST::default(); n as usize];
        let got = GetRawInputDeviceList(
            Some(list.as_mut_ptr()),
            &mut n,
            std::mem::size_of::<RAWINPUTDEVICELIST>() as u32,
        );
        if got == u32::MAX {
            return 0;
        }

        let mut count = 0u32;
        for d in &list {
            let mut info = RID_DEVICE_INFO::default();
            info.cbSize = std::mem::size_of::<RID_DEVICE_INFO>() as u32;
            let mut size = info.cbSize;
            if GetRawInputDeviceInfoW(
                Some(d.hDevice),
                RIDI_DEVICEINFO,
                Some(&mut info as *mut _ as *mut _),
                &mut size,
            ) == u32::MAX
            {
                continue;
            }
            let hid = &info.Anonymous.hid;
            // UsagePage 0x0D = Digitizers;Usage 0x04 = TouchScreen
            if hid.usUsagePage == 0x0D && hid.usUsage == 0x04 {
                count += 1;
            }
        }
        count
    }
}

impl Provider for DisplayProvider {
    fn describe(&self) -> &str {
        "win-display"
    }

    fn category(&self) -> Category {
        Category::Display
    }

    fn probe(&self) -> Result<Vec<DeviceData>, String> {
        let displays = enumerate_active()?;
        if displays.is_empty() {
            return Err("未发现活动的显示路径".into());
        }
        let touch = count_touchscreens();

        let mut devices = Vec::new();
        for (i, d) in displays.iter().enumerate() {
            let name = if d.friendly_name.is_empty() {
                format!("Display {i}")
            } else {
                d.friendly_name.clone()
            };
            let mut b = DeviceBuilder::new(format!("display:{}-{}", d.adapter_id, d.target_id), name.clone())
                .model(name.clone())
                .static_str("connector", "Connector", tech_name(d.tech))
                .static_str(
                    "monitor_name",
                    "Monitor Name",
                    if d.friendly_name.is_empty() { "Unknown" } else { &d.friendly_name },
                )
                .dynamic("resolution", "Resolution", "", ValueType::Str)
                .dynamic("refresh_hz", "Refresh Rate", "Hz", ValueType::F64);

            if touch > 0 {
                b = b.static_u64("touch_devices", "Touch Devices", "", touch as u64)
                    .static_str("touch", "Touchscreen", "Yes");
            } else {
                b = b.static_str("touch", "Touchscreen", "No");
            }
            devices.push(b.build());
        }
        Ok(devices)
    }

    fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_millis(2000)
    }

    fn refresh(&self, devices: &mut [DeviceData]) {
        // 分辨率/刷新率可被用户或 VRR 改变:刷新时重新查询
        let displays = match enumerate_active() {
            Ok(d) => d,
            Err(_) => {
                for dev in devices.iter_mut() {
                    dev.set_reading("resolution", Reading::failed(Status::Error, ValueType::Str));
                    dev.set_reading("refresh_hz", Reading::failed(Status::Error, ValueType::F64));
                }
                return;
            }
        };
        for (i, dev) in devices.iter_mut().enumerate() {
            match displays.get(i) {
                Some(d) => {
                    if d.width > 0 {
                        dev.set_reading(
                            "resolution",
                            Reading::now(Value::Str(format!("{}x{}", d.width, d.height))),
                        );
                    } else {
                        dev.set_reading("resolution", Reading::failed(Status::NotSupported, ValueType::Str));
                    }
                    match d.refresh_hz {
                        Some(hz) if hz > 1.0 => {
                            dev.set_reading("refresh_hz", Reading::now(Value::F64((hz * 1000.0).round() / 1000.0)));
                        }
                        _ => dev.set_reading("refresh_hz", Reading::failed(Status::NotSupported, ValueType::F64)),
                    }
                }
                None => {
                    // 显示器在 probe 后被拔出/禁用:标记不支持而不是输出旧值
                    dev.set_reading("resolution", Reading::failed(Status::NotSupported, ValueType::Str));
                    dev.set_reading("refresh_hz", Reading::failed(Status::NotSupported, ValueType::F64));
                }
            }
        }
    }
}
