#![cfg(target_os = "windows")]
//! root\WMI MSStorageDriver_FailurePredictData 全属性诊断
use hwprobe_core::platform::windows::shared_wmi_root_wmi;

#[derive(serde::Deserialize)]
#[serde(rename = "MSStorageDriver_FailurePredictData")]
struct SmartData {
    InstanceName: Option<String>,
    VendorSpecific: Option<Vec<u8>>,
}

fn main() {
    let Ok(conn) = shared_wmi_root_wmi() else {
        println!("root\\WMI 连接失败");
        return;
    };
    let rows: Vec<SmartData> = conn
        .raw_query("SELECT InstanceName, VendorSpecific FROM MSStorageDriver_FailurePredictData")
        .unwrap_or_default();
    println!("rows = {}", rows.len());
    for row in rows {
        println!("== {}", row.InstanceName.unwrap_or_default());
        let Some(d) = row.VendorSpecific else { continue };
        println!("   len = {}", d.len());
        if d.len() < 362 {
            continue;
        }
        for off in (2..362).step_by(12) {
            let id = d[off];
            if id == 0 {
                continue;
            }
            let raw: Vec<String> = d[off + 4..off + 12].iter().map(|b| format!("{b:02X}")).collect();
            println!(
                "   id={:3} flags={:02X}{:02X} value={:3} raw={}",
                id,
                d[off + 1],
                d[off + 2],
                d[off + 3],
                raw.join(" ")
            );
        }
    }
}
