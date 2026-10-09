#![cfg(target_os = "windows")]
//! 电池设备枚举/打开诊断。
//!
//! 背景:Claw A1M 上 provider Battery 报 "电池设备枚举失败(设备存在但打不开)",
//! 但设备管理器里 Microsoft ACPI-Compliant Control Method Battery 状态正常、
//! WMI(Win32_Battery)也能读到电量。需要区分是
//!   (a) 枚举不到设备接口路径,还是
//!   (b) 枚举到了但 CreateFile 打不开(以及具体是哪个 Win32 错误)。
//!
//! 本程序对同一台机器走三条独立的枚举通路并逐一尝试多种访问权限组合,
//! 打印确切的 Win32 错误码。
//!
//! 用法: dbg18

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDeviceRegistryPropertyW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SPDRP_FRIENDLYNAME,
};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

// GUID_DEVCLASS_BATTERY == GUID_DEVINTERFACE_BATTERY {72631E54-78A4-11D0-BCF7-00AA00B7B32A}
const GUID_BATTERY: GUID = GUID::from_u128(0x72631E54_78A4_11D0_BCF7_00AA00B7B32A);

const FILE_DEVICE_BATTERY: u32 = 0x0000_0029;
const fn ctl(dev: u32, func: u32, access: u32) -> u32 {
    (dev << 16) | (access << 14) | (func << 2)
}
// poclass.h 的权威定义:三条 IOCTL 的访问位都是 FILE_READ_ACCESS(1)。
// provider 把 QUERY_TAG 写成了 READ|WRITE(3) —— 本诊断同时验证两种。
const IOCTL_QUERY_TAG_RW: u32 = ctl(FILE_DEVICE_BATTERY, 0x10, 3); // provider 现状
const IOCTL_QUERY_TAG_R: u32 = ctl(FILE_DEVICE_BATTERY, 0x10, 1); // poclass.h 定义
const IOCTL_QUERY_INFORMATION: u32 = ctl(FILE_DEVICE_BATTERY, 0x11, 1);
const IOCTL_QUERY_STATUS: u32 = ctl(FILE_DEVICE_BATTERY, 0x13, 1);

// BATTERY_QUERY_INFORMATION_LEVEL
const BATTERY_INFO_INFORMATION: u32 = 0;
const BATTERY_INFO_DEVICE_NAME: u32 = 4;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BatteryWaitTag {
    tag: u32,
    timeout: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BatteryQueryInfoInput {
    tag: u32,
    level: u32,
    at_rate: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BatteryWaitStatus {
    tag: u32,
    timeout: u32,
    power_state: u32,
    low_capacity: u32,
    high_capacity: u32,
}

/// 调一次 DeviceIoControl,返回 Ok(实际返回字节数) 或 Err(裸 Win32 错误码)。
unsafe fn ioctl(
    h: HANDLE,
    code: u32,
    input: Option<(*const u8, u32)>,
    out: Option<(*mut u8, u32)>,
) -> Result<u32, u32> {
    let mut ret = 0u32;
    let (inp, inlen) = input.map_or((None, 0), |(p, l)| (Some(p as *const _), l));
    let (outp, outlen) = out.map_or((None, 0), |(p, l)| (Some(p as *mut _), l));
    match DeviceIoControl(h, code, inp, inlen, outp, outlen, Some(&mut ret), None) {
        Ok(()) => Ok(ret),
        Err(e) => Err(raw_win32(&e)),
    }
}

/// 按 provider 的调用方式逐个 IOCTL 复现,定位失败发生在哪一步。
fn ioctl_sequence(path: &str) {
    let h = match create(
        path,
        GENERIC_READ.0 | GENERIC_WRITE.0,
        FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0,
    ) {
        Ok(h) => h,
        Err(e) => {
            println!("  CreateFile 失败: {e}");
            return;
        }
    };

    unsafe {
        // 1) QUERY_TAG —— 分别用 provider 现状的访问位(3) 与 poclass.h 定义的(1),
        //    输入也各试 8 字节 BatteryWaitTag 与 4 字节超时。
        let mut wait = BatteryWaitTag { tag: 0, timeout: 0 };
        let mut timeout_only: u32 = 0;

        let try_tag = |code: u32, label: &str, input: (*const u8, u32)| -> u32 {
            let mut t: u32 = 0;
            let r = ioctl(h, code, Some(input), Some((&mut t as *mut _ as *mut u8, 4)));
            println!("  QUERY_TAG {label}: {r:?}  tag={t}");
            t
        };

        let a = try_tag(IOCTL_QUERY_TAG_RW, "访问位=3, 8 字节输入 (provider 现状)", (&mut wait as *mut _ as *const u8, 8));
        let b = try_tag(IOCTL_QUERY_TAG_RW, "访问位=3, 4 字节输入", (&mut timeout_only as *mut _ as *const u8, 4));
        let c = try_tag(IOCTL_QUERY_TAG_R, "访问位=1, 4 字节输入 (poclass.h)", (&mut timeout_only as *mut _ as *const u8, 4));
        let d = try_tag(IOCTL_QUERY_TAG_R, "访问位=1, 8 字节输入", (&mut wait as *mut _ as *const u8, 8));

        let tag_use = [a, b, c, d].into_iter().find(|t| *t != 0).unwrap_or(0);
        println!("  -> 后续查询使用 tag={tag_use}  (a={a} b={b} c={c} d={d})");

        // 2) QUERY_INFORMATION: BatteryDeviceName(4)
        let mut name_in = BatteryQueryInfoInput { tag: tag_use, level: BATTERY_INFO_DEVICE_NAME, at_rate: 0 };
        let mut namebuf = [0u8; 256];
        let rn = ioctl(
            h,
            IOCTL_QUERY_INFORMATION,
            Some((&mut name_in as *mut _ as *const u8, 12)),
            Some((&mut namebuf as *mut _ as *mut u8, 256)),
        );
        let name = String::from_utf16_lossy(
            &namebuf
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|c| *c != 0)
                .collect::<Vec<u16>>(),
        );
        println!("  QUERY_INFORMATION(BatteryDeviceName): {rn:?}  \"{name}\"");

        // 3) QUERY_INFORMATION: BatteryInformation(0)
        let mut info_in = BatteryQueryInfoInput { tag: tag_use, level: BATTERY_INFO_INFORMATION, at_rate: 0 };
        let mut infobuf = [0u8; 64];
        let ri = ioctl(
            h,
            IOCTL_QUERY_INFORMATION,
            Some((&mut info_in as *mut _ as *const u8, 12)),
            Some((&mut infobuf as *mut _ as *mut u8, 64)),
        );
        let caps = u32::from_le_bytes([infobuf[0], infobuf[1], infobuf[2], infobuf[3]]);
        let tech = u32::from_le_bytes([infobuf[4], infobuf[5], infobuf[6], infobuf[7]]);
        let design = u32::from_le_bytes([infobuf[12], infobuf[13], infobuf[14], infobuf[15]]);
        let full = u32::from_le_bytes([infobuf[16], infobuf[17], infobuf[18], infobuf[19]]);
        println!(
            "  QUERY_INFORMATION(BatteryInformation): {ri:?}  capabilities=0x{caps:X} technology={tech} design={design} full={full}"
        );

        // 4) QUERY_STATUS
        let mut ws = BatteryWaitStatus { tag: tag_use, ..Default::default() };
        let mut status = [0u8; 16];
        let rs = ioctl(
            h,
            IOCTL_QUERY_STATUS,
            Some((&mut ws as *mut _ as *const u8, std::mem::size_of::<BatteryWaitStatus>() as u32)),
            Some((&mut status as *mut _ as *mut u8, 16)),
        );
        let power = u32::from_le_bytes([status[0], status[1], status[2], status[3]]);
        let cap = u32::from_le_bytes([status[4], status[5], status[6], status[7]]);
        let volt = u32::from_le_bytes([status[8], status[9], status[10], status[11]]);
        let rate = i32::from_le_bytes([status[12], status[13], status[14], status[15]]);
        println!("  QUERY_STATUS: {rs:?}  power_state=0x{power:X} capacity={cap} voltage={volt}mV rate={rate}mW");

        let _ = CloseHandle(h);
    }
}

/// 常见 Win32 错误码,便于读结果。
fn win32_name(code: u32) -> &'static str {
    match code {
        2 => "FILE_NOT_FOUND",
        3 => "PATH_NOT_FOUND",
        5 => "ACCESS_DENIED",
        31 => "GEN_FAILURE",
        32 => "SHARING_VIOLATION",
        87 => "INVALID_PARAMETER",
        1167 => "DEVICE_NOT_CONNECTED",
        1168 => "NOT_FOUND",
        _ => "",
    }
}

/// windows crate 把 Win32 失败包成 HRESULT_FROM_WIN32,这里还原成裸错误码。
fn raw_win32(e: &windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 {
        hr & 0xFFFF
    } else {
        hr
    }
}

fn report(tag: &str, r: Result<HANDLE, windows::core::Error>) -> Option<HANDLE> {
    match r {
        Ok(h) => {
            println!("      {tag:<46} OK  (handle 0x{:X})", h.0 as usize);
            Some(h)
        }
        Err(e) => {
            let c = raw_win32(&e);
            println!("      {tag:<46} 失败 code={c} {} ({e})", win32_name(c));
            None
        }
    }
}

fn create(path: &str, access: u32, share: u32) -> Result<HANDLE, windows::core::Error> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            access,
            windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(share),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
}

/// 通路 1:SetupDi 设备接口枚举(provider 当前用的方式)。
fn setupdi_interfaces() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let hdev = match SetupDiGetClassDevsW(
            Some(&GUID_BATTERY),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) {
            Ok(h) if !h.is_invalid() => h,
            other => {
                println!("  SetupDiGetClassDevsW 失败: {other:?}");
                return out;
            }
        };
        let mut i = 0u32;
        loop {
            let mut ifdata = SP_DEVICE_INTERFACE_DATA {
                cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(hdev, None, &GUID_BATTERY, i, &mut ifdata).is_err() {
                break;
            }
            let mut needed = 0u32;
            let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &ifdata, None, 0, Some(&mut needed), None);
            if needed == 0 {
                i += 1;
                continue;
            }
            let mut buf = vec![0u8; needed as usize];
            buf[0..4].copy_from_slice(&8u32.to_le_bytes()); // x64 上 cbSize 必须为 8
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            if SetupDiGetDeviceInterfaceDetailW(hdev, &ifdata, Some(detail), needed, None, None).is_ok()
            {
                let mut path = String::new();
                let mut p = (detail as *const u8).add(4) as *const u16;
                for _ in 0..1024 {
                    let c = *p;
                    if c == 0 {
                        break;
                    }
                    path.push(char::from_u32(c as u32).unwrap_or('?'));
                    p = p.add(1);
                }
                println!("  [{i}] {path}");
                out.push(path);
            }
            i += 1;
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    out
}

/// 通路 2:SetupDi 设备类成员枚举(不需要设备暴露接口)。
fn setupdi_class_members() {
    unsafe {
        let hdev = match SetupDiGetClassDevsW(Some(&GUID_BATTERY), None, None, DIGCF_PRESENT) {
            Ok(h) if !h.is_invalid() => h,
            other => {
                println!("  SetupDiGetClassDevsW(类枚举) 失败: {other:?}");
                return;
            }
        };
        let mut i = 0u32;
        loop {
            let mut di = SP_DEVINFO_DATA {
                cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(hdev, i, &mut di).is_err() {
                break;
            }
            let mut id = [0u16; 512];
            let mut idlen = 0u32;
            let inst = if SetupDiGetDeviceInstanceIdW(hdev, &di, Some(&mut id), Some(&mut idlen)).is_ok() {
                String::from_utf16_lossy(&id[..idlen.saturating_sub(1) as usize])
            } else {
                "<无法取实例 ID>".into()
            };
            let mut name = [0u8; 512];
            let mut nlen = 0u32;
            let friendly = if SetupDiGetDeviceRegistryPropertyW(
                hdev,
                &di,
                SPDRP_FRIENDLYNAME,
                None,
                Some(&mut name),
                Some(&mut nlen),
            )
            .is_ok()
            {
                let u16s: Vec<u16> = name[..nlen as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                String::from_utf16_lossy(&u16s).trim_end_matches('\0').to_string()
            } else {
                "<无友好名>".into()
            };
            println!("  [{i}] {inst}   \"{friendly}\"");
            i += 1;
        }
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
}

/// 通路 3:CfgMgr32 的设备接口列表(独立于 SetupDi 的实现)。
fn cfgmgr_interfaces() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let mut len = 0u32;
        let cr = CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &GUID_BATTERY,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if cr != CR_SUCCESS {
            println!("  CM_Get_Device_Interface_List_SizeW 失败: CONFIGRET={}", cr.0);
            return out;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let cr = CM_Get_Device_Interface_ListW(
            &GUID_BATTERY,
            PCWSTR::null(),
            &mut buf,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if cr != CR_SUCCESS {
            println!("  CM_Get_Device_Interface_ListW 失败: CONFIGRET={}", cr.0);
            return out;
        }
        // 多字符串(双 NUL 结尾)
        let mut cur = Vec::new();
        for &c in buf.iter() {
            if c == 0 {
                if cur.is_empty() {
                    break;
                }
                let s = String::from_utf16_lossy(&cur);
                println!("  {}", s);
                out.push(s);
                cur.clear();
            } else {
                cur.push(c);
            }
        }
    }
    out
}

fn main() {
    println!("===== 1) SetupDi 设备接口枚举 (DIGCF_PRESENT|DIGCF_DEVICEINTERFACE) =====");
    let di_paths = setupdi_interfaces();
    println!("  -> {} 个接口路径", di_paths.len());

    println!();
    println!("===== 2) SetupDi 设备类成员枚举 (DIGCF_PRESENT) =====");
    setupdi_class_members();

    println!();
    println!("===== 3) CfgMgr32 设备接口列表 (CM_Get_Device_Interface_ListW) =====");
    let cm_paths = cfgmgr_interfaces();
    println!("  -> {} 个接口路径", cm_paths.len());

    println!();
    println!("===== 4) CreateFile 访问权限组合探测 =====");
    let mut all: Vec<String> = di_paths.clone();
    for p in &cm_paths {
        if !all.contains(p) {
            all.push(p.clone());
        }
    }
    if all.is_empty() {
        println!("  (没有任何接口路径可试)");
    }
    for path in &all {
        println!("  {path}");
        let combos: [(&str, u32, u32); 5] = [
            ("READ|WRITE, share R|W (provider 当前用法)", GENERIC_READ.0 | GENERIC_WRITE.0, FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            ("READ,       share R|W", GENERIC_READ.0, FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            ("READ|WRITE, share none (独占)", GENERIC_READ.0 | GENERIC_WRITE.0, 0),
            ("READ_ATTRIBUTES, share R|W", FILE_READ_ATTRIBUTES.0, FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            ("0 (无访问权),  share R|W", 0, FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
        ];
        for (tag, access, share) in combos {
            if let Some(h) = report(tag, create(path, access, share)) {
                // 打开成功就顺手读一次 tag,确认是真电池而不是空壳接口
                unsafe {
                    let mut tag: u32 = 0;
                    let mut ret = 0u32;
                    let ok = DeviceIoControl(
                        h,
                        IOCTL_QUERY_TAG_R,
                        None,
                        0,
                        Some(&mut tag as *mut _ as *mut _),
                        4,
                        Some(&mut ret),
                        None,
                    );
                    println!("      └ IOCTL_BATTERY_QUERY_TAG: ok={} tag={tag}", ok.is_ok());
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    println!();
    println!("===== 6) 按 provider 的调用序列逐个 IOCTL 复现 =====");
    for path in &all {
        println!("  {path}");
        ioctl_sequence(path);
    }

    println!();
    println!("===== 5) 参考:GetSystemPowerStatus =====");
    let mut sps = SYSTEM_POWER_STATUS::default();
    match unsafe { GetSystemPowerStatus(&mut sps) } {
        Ok(()) => println!(
            "  ACLineStatus={} BatteryFlag={} BatteryLifePercent={}%",
            sps.ACLineStatus, sps.BatteryFlag, sps.BatteryLifePercent
        ),
        Err(e) => println!("  GetSystemPowerStatus 失败: {e}"),
    }
}
