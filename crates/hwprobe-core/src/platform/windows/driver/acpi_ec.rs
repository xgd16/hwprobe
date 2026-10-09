//! ACPI 嵌入式控制器(EC)读取:标准 ACPI 协议单字节读(RD_EC 0x80)。
//!
//! 端口:0x62 = 数据口,0x66 = 命令/状态口。底层端口原语来自
//! [`HwAccess::ec_read_port`] / [`HwAccess::ec_write_port`]:
//! - PawnIO 后端经官方 LpcACPIEC 模块(白名单仅 0x62/0x66,见 pawnio.rs);
//! - WinRing0 后端直接端口 I/O。
//!
//! 与 LibreHardwareMonitor 的 EmbeddedController 相同,每次事务前尝试获取
//! `Access_EC` 命名互斥体(部分厂商软件用它序列化 EC 访问);互斥体不存在
//! 或等待超时则继续(事务很短,逐寄存器范围校验兜底)。
//!
//! 只读:本模块刻意不实现写侧(写入 EC 控制寄存器属于系统策略行为,
//! 超出监测库职责)。

#![cfg(target_os = "windows")]

use super::HwAccess;
use std::time::{Duration, Instant};

const EC_DATA: u16 = 0x62;
const EC_SC: u16 = 0x66;
/// 状态寄存器位:输出缓冲满 / 输入缓冲满。
const EC_SC_OBF: u8 = 0x01;
const EC_SC_IBF: u8 = 0x02;
/// ACPI 协议命令:读 EC 寄存器。
const EC_CMD_RD_EC: u8 = 0x80;

/// 单次端口等待预算(EC 响应通常 < 1ms;超时视为事务失败)。
const WAIT_BUDGET: Duration = Duration::from_millis(30);
/// Access_EC 互斥体等待时长。
const MUTEX_WAIT_MS: u32 = 20;

pub struct AcpiEc {
    access: &'static dyn HwAccess,
    mutex: Option<windows::Win32::Foundation::HANDLE>,
    /// 互斥体等待失败只提示一次。
    mutex_warned: std::sync::atomic::AtomicBool,
}

unsafe impl Send for AcpiEc {}
unsafe impl Sync for AcpiEc {}

impl AcpiEc {
    /// 打开 EC 通道:要求驱动层可用,并做存在性自检
    /// (状态口可读且非持续 0xFF;完成一次真实 RD_EC 事务)。
    pub fn open() -> Option<AcpiEc> {
        let access = super::access()?;

        // 状态口存在性:连续 0xFF 通常表示端口无设备
        let mut alive = false;
        for _ in 0..3 {
            if let Some(s) = access.ec_read_port(EC_SC) {
                if s != 0xFF {
                    alive = true;
                    break;
                }
            }
        }
        if !alive {
            crate::logbuf::info("ACPI EC 状态口无响应(0x66 恒为 0xFF),EC 传感器不可用");
            return None;
        }

        let ec = AcpiEc {
            access,
            mutex: open_access_mutex(),
            mutex_warned: std::sync::atomic::AtomicBool::new(false),
        };
        // 真实事务自检:寄存器 0x00(电池状态块)任何值都算成功
        if ec.read(0x00).is_none() {
            // 单独记一条:状态口有响应但读事务超时(IBF/OBF 不翻转),
            // 与"驱动层整个不可用"是两种不同故障
            crate::logbuf::info("ACPI EC 状态口可读,但 RD_EC 事务无响应,EC 传感器不可用");
            return None;
        }
        Some(ec)
    }

    /// 读一个 EC 寄存器。失败返回 None(不重试,由上层决定降级策略)。
    pub fn read(&self, addr: u8) -> Option<u8> {
        let locked = self.lock();
        let result = self.transaction(addr);
        if locked {
            unsafe {
                let _ = windows::Win32::System::Threading::ReleaseMutex(self.mutex.unwrap());
            }
        }
        result
    }

    fn transaction(&self, addr: u8) -> Option<u8> {
        // 写命令前等 IBF 清空
        if !self.wait_status(|s| s & EC_SC_IBF == 0) {
            return None;
        }
        self.access.ec_write_port(EC_SC, EC_CMD_RD_EC);
        if !self.wait_status(|s| s & EC_SC_IBF == 0) {
            return None;
        }
        self.access.ec_write_port(EC_DATA, addr);
        // 等 OBF 置位后读数据
        if !self.wait_status(|s| s & EC_SC_OBF != 0) {
            return None;
        }
        self.access.ec_read_port(EC_DATA)
    }

    /// 轮询状态口直到谓词满足或超时。
    fn wait_status(&self, pred: impl Fn(u8) -> bool) -> bool {
        let start = Instant::now();
        loop {
            if let Some(s) = self.access.ec_read_port(EC_SC) {
                if pred(s) {
                    return true;
                }
            }
            if start.elapsed() >= WAIT_BUDGET {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// 尝试获取 Access_EC 互斥体。返回 true 表示已持有(读后需释放)。
    fn lock(&self) -> bool {
        let Some(m) = self.mutex else {
            return false;
        };
        unsafe {
            use windows::Win32::Foundation::WAIT_TIMEOUT;
            match windows::Win32::System::Threading::WaitForSingleObject(m, MUTEX_WAIT_MS) {
                windows::Win32::Foundation::WAIT_OBJECT_0 => true,
                WAIT_TIMEOUT => {
                    if !self.mutex_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        crate::logbuf::info(
                            "Access_EC 互斥体等待超时,继续无锁读取(读值经范围校验)",
                        );
                    }
                    false
                }
                _ => false,
            }
        }
    }
}

impl Drop for AcpiEc {
    fn drop(&mut self) {
        if let Some(m) = self.mutex {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(m);
            }
        }
    }
}

/// 打开 Access_EC 命名互斥体(不存在 = 无竞争方,直接返回 None)。
fn open_access_mutex() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

    let name: Vec<u16> = "Access_EC\0".encode_utf16().collect();
    unsafe {
        OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr()))
            .ok()
            .map(|h| h.into())
    }
}
