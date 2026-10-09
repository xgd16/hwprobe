//! PawnIO 官方安装器的一键静默安装。
//!
//! PawnIO 内核驱动本体必须注册为系统服务后由内核加载(Windows 硬性规则),
//! 无法随 DLL 免安装使用。因此需要一个官方签名安装器(namazso/PawnIO.Setup,
//! 来源与 SHA-256 见 `assets/PawnIO_setup.README`),由调用方**显式**触发:
//! DLL 导出 `hwprobe_setup_driver` 或 CLI 的 `setup` 子命令。绝不在
//! `hwprobe_init` 中自动安装 —— 静默安装内核驱动必须经用户知情同意,
//! UAC 弹窗本身就是确认环节。
//!
//! 安装器参数 `-install -silent`;`runas` 提权后由安装器完成服务注册,
//! UAC 界面会显示官方签名发布者(namazso.eu)。安装完成后,本进程内已
//! 缓存的驱动初始化结果不会自动重试,需重启进程生效;访问 PawnIO 设备
//! 仍需管理员权限。
//!
//! # 安装器从哪来
//!
//! 这个 3.4 MB 的签名可执行文件**不随仓库分发**,由 `tool/fetch_assets.ps1`
//! 按需下载并校验。因此有两种取用方式,按顺序尝试:
//!
//! 1. **编译期内嵌**:构建时若 `assets/PawnIO_setup.exe` 存在,就把它嵌进
//!    二进制(见 `build.rs` 设置的 `hwprobe_embedded_pawnio_setup`),运行时
//!    无需任何外部文件 —— 这是发布构建的形态。
//! 2. **运行期从磁盘找**:未内嵌时(没跑过 fetch_assets 就构建),依次在
//!    `HWPROBE_PAWNIO_SETUP` 环境变量、exe 同目录、当前工作目录、
//!    `assets/` 下寻找安装器。
//!
//! 两者都没有时返回一条可操作的错误,而不是构建失败 —— 缺安装器不该
//! 阻止别人编译这个库。

use crate::logbuf;

/// 内嵌的官方安装器字节。仅当构建时文件存在才存在(见 `build.rs`)。
#[cfg(hwprobe_embedded_pawnio_setup)]
const INSTALLER: &[u8] = include_bytes!("../../../../assets/PawnIO_setup.exe");

const INSTALLER_FILE: &str = "PawnIO_setup.exe";
const SERVICE_NAME: &str = "PawnIO";

/// 允许调用方指定安装器位置的逃生口(便于没跑 fetch_assets 的便携部署)。
const ENV_INSTALLER: &str = "HWPROBE_PAWNIO_SETUP";

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

/// 安装 PawnIO(幂等)。已安装直接返回 Ok;否则取得官方安装器并以
/// UAC 提权方式静默安装,等待结束后确认服务存在。
pub fn install_pawnio() -> Result<(), String> {
    if pawnio_installed() {
        logbuf::info("PawnIO 已安装,无需重复安装");
        return Ok(());
    }
    let path = obtain_installer()?;
    logbuf::info("运行 PawnIO 官方安装器(UAC 提权 + 静默)…");
    run_installer(&path)?;
    if !pawnio_installed() {
        return Err("安装器已退出,但未检测到 PawnIO 服务".into());
    }
    logbuf::info("PawnIO 安装成功");
    Ok(())
}

/// 把安装器准备到磁盘上并返回其路径:内嵌字节优先(释放到临时目录),
/// 否则在常见位置里找一个可直接运行的。
fn obtain_installer() -> Result<String, String> {
    if let Some(bytes) = embedded_installer() {
        return extract_embedded(bytes);
    }
    if let Some(found) = locate_installer_on_disk() {
        logbuf::info(format!("使用磁盘上的 PawnIO 安装器: {found}"));
        return Ok(found);
    }
    Err(format!(
        "未找到 PawnIO 安装器({INSTALLER_FILE})。请先运行 \
         `powershell -ExecutionPolicy Bypass -File tool/fetch_assets.ps1` 下载并校验, \
         或用环境变量 {ENV_INSTALLER} 指向已有的官方安装器。"
    ))
}

#[cfg(hwprobe_embedded_pawnio_setup)]
fn embedded_installer() -> Option<&'static [u8]> {
    Some(INSTALLER)
}

#[cfg(not(hwprobe_embedded_pawnio_setup))]
fn embedded_installer() -> Option<&'static [u8]> {
    None
}

fn extract_embedded(bytes: &[u8]) -> Result<String, String> {
    let dir = std::env::temp_dir().join("hwprobe");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let path = dir.join(INSTALLER_FILE);
    // 每次都重写,避免残留旧版本/被替换的文件
    std::fs::write(&path, bytes).map_err(|e| format!("写出安装器失败: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

/// 按优先级在磁盘上寻找安装器。
fn locate_installer_on_disk() -> Option<String> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    if let Some(explicit) = std::env::var_os(ENV_INSTALLER) {
        candidates.push(std::path::PathBuf::from(explicit));
    }
    // exe 同目录:便携部署把安装器和应用放在一起
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(INSTALLER_FILE));
        }
    }
    // 当前工作目录的常见位置:从仓库根或 crates/ 下运行 CLI 都能命中
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join(INSTALLER_FILE));
        candidates.push(cwd.join("assets").join(INSTALLER_FILE));
        candidates.push(
            cwd.join("crates")
                .join("hwprobe-core")
                .join("assets")
                .join(INSTALLER_FILE),
        );
    }

    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().to_string())
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
