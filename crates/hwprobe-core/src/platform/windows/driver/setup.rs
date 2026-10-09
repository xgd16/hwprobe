//! PawnIO 官方安装器的一键静默安装。
//!
//! PawnIO 内核驱动本体必须注册为系统服务后由内核加载(Windows 硬性规则),
//! 无法随 DLL 免安装使用。因此随包内置官方签名安装器(namazso/PawnIO.Setup,
//! 来源与 SHA-256 见 `assets/PawnIO_setup.README`),由调用方**显式**触发:
//! DLL 导出 `hwprobe_setup_driver` 或 CLI 的 `setup` 子命令。绝不在
//! `hwprobe_init` 中自动安装 —— 静默安装内核驱动必须经用户知情同意,
//! UAC 弹窗本身就是确认环节。
//!
//! 安装器参数 `-install -silent`;`runas` 提权后由安装器完成服务注册,
//! UAC 界面会显示官方签名发布者(namazso.eu)。安装完成后,本进程内已
//! 缓存的驱动初始化结果不会自动重试,需重启进程生效;访问 PawnIO 设备
//! 仍需管理员权限。

use crate::logbuf;

const INSTALLER: &[u8] = include_bytes!("../../../../assets/PawnIO_setup.exe");
const SERVICE_NAME: &str = "PawnIO";

/// 查询 PawnIO 驱动是否已安装(服务存在即视为已安装;不要求管理员)。
pub fn pawnio_installed() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Services::*;

    unsafe {
        let Ok(manager) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
            return false;
        };
        let name = wide(SERVICE_NAME);
        let installed = match OpenServiceW(manager, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS) {
            Ok(svc) => {
                CloseServiceHandle(svc).ok();
                true
            }
            Err(_) => false,
        };
        CloseServiceHandle(manager).ok();
        installed
    }
}

/// 安装 PawnIO(幂等)。已安装直接返回 Ok;否则释放内置官方安装器并以
/// UAC 提权方式静默安装,等待结束后确认服务存在。
pub fn install_pawnio() -> Result<(), String> {
    if pawnio_installed() {
        logbuf::info("PawnIO 已安装,无需重复安装");
        return Ok(());
    }
    let path = extract_installer()?;
    logbuf::info("运行 PawnIO 官方安装器(UAC 提权 + 静默)…");
    run_installer(&path)?;
    if !pawnio_installed() {
        return Err("安装器已退出,但未检测到 PawnIO 服务".into());
    }
    logbuf::info("PawnIO 安装成功");
    Ok(())
}

fn extract_installer() -> Result<String, String> {
    let dir = std::env::temp_dir().join("hwprobe");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let path = dir.join("PawnIO_setup.exe");
    // 每次都重写,避免残留旧版本/被替换的文件
    std::fs::write(&path, INSTALLER).map_err(|e| format!("写出安装器失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn run_installer(path: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let verb = wide("runas");
    let file = wide(path);
    let params = wide("-install -silent");

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut sei) {
            return Err(format!("启动安装器失败(可能拒绝了 UAC 提权): {e}"));
        }
        let h = sei.hProcess;
        if WaitForSingleObject(h, 120_000) != WAIT_OBJECT_0 {
            let _ = CloseHandle(h);
            return Err("等待安装器超时(120s)".into());
        }
        let mut code = 0u32;
        let _ = GetExitCodeProcess(h, &mut code);
        let _ = CloseHandle(h);
        if code != 0 {
            return Err(format!("安装器退出码 {code}(可能拒绝了 UAC 或被策略拦截)"));
        }
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
