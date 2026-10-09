//! SMBus 访问原语:内存 SPD / 温度传感器读取通路。
//!
//! 后端是 PawnIO 的 SMBus 控制器模块(i801 / PIIX4 / NCT6793),需要管理员
//! 权限;驱动或模块不可用时 [`buses`] 返回空,上层不声明相关传感器。
//!
//! 传输前按业界惯例持有 `Global\Access_SMBUS.HTP.Method` 互斥体(HWiNFO /
//! AIDA64 / LibreHardwareMonitor 等同样使用),避免多程序并发操作 SMBus
//! 控制器。拿不到锁(其他程序长期持有 / 权限不足)时最多等 1 秒后继续,
//! 不阻塞后台轮询线程。

#![cfg(target_os = "windows")]

use super::driver;
use crate::logbuf;
use std::sync::OnceLock;

/// SMBus 协议码(与 Linux i2c_smbus / PawnIO 模块一致)。
#[allow(dead_code)]
pub const PROTO_QUICK: u8 = 0;
#[allow(dead_code)]
pub const PROTO_BYTE: u8 = 1;
pub const PROTO_BYTE_DATA: u8 = 2;
pub const PROTO_WORD_DATA: u8 = 3;

/// 一条可用的 SMBus 总线。
#[derive(Clone, Copy, Debug)]
pub struct Bus {
    pub index: usize,
    pub name: &'static str,
}

/// 当前可用的 SMBus 总线列表(驱动不可用或控制器未识别时为空)。
pub fn buses() -> Vec<Bus> {
    let Some(drv) = driver::access() else {
        return Vec::new();
    };
    (0..drv.smbus_bus_count())
        .map(|i| Bus {
            index: i,
            name: drv.smbus_bus_name(i),
        })
        .collect()
}

impl Bus {
    /// SMBus Read Byte Data(协议 2)。
    pub fn read_byte_data(self, addr: u8, command: u8) -> Option<u8> {        let _guard = SmbusLock::acquire();
        driver::access()?
            .smbus_xfer(self.index, addr, true, command, PROTO_BYTE_DATA, 0)
            .map(|v| v as u8)
    }

    /// SMBus Write Byte Data(协议 2)。
    pub fn write_byte_data(self, addr: u8, command: u8, value: u8) -> bool {
        let _guard = SmbusLock::acquire();
        match driver::access() {
            Some(drv) => drv
                .smbus_xfer(self.index, addr, false, command, PROTO_BYTE_DATA, value as u16)
                .is_some(),
            None => false,
        }
    }

    /// SMBus Quick Write(协议 0):只发地址 + 写位,用于探测设备是否存在。
    pub fn quick_write(self, addr: u8) -> bool {
        let _guard = SmbusLock::acquire();
        match driver::access() {
            Some(drv) => drv
                .smbus_xfer(self.index, addr, false, 0, PROTO_QUICK, 0)
                .is_some(),
            None => false,
        }
    }

    /// SMBus Read Word Data(协议 3,低字节在前)。
    pub fn read_word_data(self, addr: u8, command: u8) -> Option<u16> {
        let _guard = SmbusLock::acquire();
        driver::access()?
            .smbus_xfer(self.index, addr, true, command, PROTO_WORD_DATA, 0)
    }

    /// 大字端字读取:先按标准读字,再交换字节。
    /// DDR4 温度传感器(TSOD)的寄存器就是大字端(RAMSPDToolkit 的
    /// `i2c_smbus_read_word_data_swapped`,与 Linux jc42 驱动一致)。
    pub fn read_word_data_swapped(self, addr: u8, command: u8) -> Option<u16> {
        self.read_word_data(addr, command).map(u16::swap_bytes)
    }
}

// ---------------- 全局 SMBus 互斥体 ----------------

/// 句柄包一层以满足 Send + Sync(进程生命周期内不关闭)。
struct MutexHandle(windows::Win32::Foundation::HANDLE);
unsafe impl Send for MutexHandle {}
unsafe impl Sync for MutexHandle {}

static SMBUS_MUTEX: OnceLock<Option<MutexHandle>> = OnceLock::new();

struct SmbusLock {
    handle: Option<windows::Win32::Foundation::HANDLE>,
    acquired: bool,
}

impl SmbusLock {
    fn acquire() -> SmbusLock {
        use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

        let slot = SMBUS_MUTEX.get_or_init(|| unsafe {
            let name: Vec<u16> = "Global\\Access_SMBUS.HTP.Method"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            match CreateMutexW(None, false, windows::core::PCWSTR(name.as_ptr())) {
                Ok(h) => Some(MutexHandle(h)),
                Err(e) => {
                    // 已存在但 ACL 受限时会走到这里;无锁继续,仅提示一次
                    logbuf::info(format!("SMBus 互斥体不可用(继续无锁访问): {e}"));
                    None
                }
            }
        });

        let Some(MutexHandle(handle)) = slot else {
            return SmbusLock {
                handle: None,
                acquired: false,
            };
        };
        let handle = *handle;
        let r = unsafe { WaitForSingleObject(handle, 1000) };
        SmbusLock {
            handle: Some(handle),
            acquired: r == WAIT_OBJECT_0 || r == WAIT_ABANDONED,
        }
    }
}

impl Drop for SmbusLock {
    fn drop(&mut self) {
        if self.acquired {
            if let Some(h) = self.handle {
                unsafe {
                    windows::Win32::System::Threading::ReleaseMutex(h).ok();
                }
            }
        }
    }
}
