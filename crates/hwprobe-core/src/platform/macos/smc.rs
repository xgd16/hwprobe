//! AppleSMC user client 封装:温度 / 功耗 / 风扇等硬件传感器的取数通道。
//!
//! macOS 的 SMC(System Management Controller)由 `AppleSMC` 驱动暴露为
//! IOService user client,经 `IOConnectCallStructMethod(selector = 2)` 收发
//! 定长 80 字节的 `SmcKeyData` 结构读写按键。这与 HWiNFO / smcFanControl /
//! LibreHardwareMonitor 之外的 macOS 工具(smc-command、Stats、macmon)走的是
//! 同一条公开通道,**不需要内核驱动,大多数键普通用户可读**。
//!
//! 数值类型解码按业界惯例:
//! - `sp78`  :16 位大端有符号定点(7.8),值 = i16 / 256(温度 °C);
//! - `fpe2`  :16 位大端无符号定点(14.2),值 = u16 / 4(风扇 RPM);
//! - `flt `  :32 位小端浮点(Apple Silicon 常见,功耗 W 等);
//! - `ui8/ui16/ui32`、`i8/i16/i32`:大端整数;
//! - `flag`  :布尔;`ch8 ` / `{fds`:字符/结构体原始字节。
//!
//! 读不到的键就是读不到(机型没有该键 / 无权限 / SMC 拒绝),一律 `None`,
//! 由调用方决定不声明该传感器 —— 不猜值、不填 0。

use super::iokit::{matching_service, IoObject};
use io_kit_sys::types::io_connect_t;
use io_kit_sys::{IOConnectCallStructMethod, IOServiceClose, IOServiceOpen};
use mach2::traps::mach_task_self;

const KERNEL_INDEX_SMC: u32 = 2;
const SMC_CMD_READ_BYTES: u8 = 5;
const SMC_CMD_READ_INDEX: u8 = 8;
const SMC_CMD_READ_KEYINFO: u8 = 9;

/// SMC 单次可读的最大字节数。
const SMC_MAX_DATA: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmcVersion {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmcPLimitData {
    version: u16,
    length: u16,
    cpu_plimit: u32,
    gpu_plimit: u32,
    mem_plimit: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmcKeyInfoData {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

/// `AppleSMC` user client 的入/出参结构(C 端 80 字节,布局由 repr(C) 天然对齐)。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SmcKeyData {
    key: u32,
    vers: SmcVersion,
    p_limit_data: SmcPLimitData,
    key_info: SmcKeyInfoData,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; SMC_MAX_DATA],
}

const _: () = assert!(std::mem::size_of::<SmcKeyData>() == 80);

/// 已解码的 SMC 值。
#[derive(Debug, Clone)]
pub enum SmcVal {
    F64(f64),
    U64(u64),
    I64(i64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
}

impl SmcVal {
    /// 数值视图(布尔按 0/1)。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SmcVal::F64(v) => Some(*v),
            SmcVal::U64(v) => Some(*v as f64),
            SmcVal::I64(v) => Some(*v as f64),
            SmcVal::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
}

/// 一个 SMC 连接句柄(进程内复用一个即可)。
pub struct Smc {
    conn: io_connect_t,
    svc: IoObject,
}

impl Smc {
    /// 打开 `AppleSMC` user client。无 SMC 服务(虚拟机等)返回 Err。
    pub fn open() -> Result<Smc, String> {
        let svc: IoObject = matching_service("AppleSMC")
            .ok_or_else(|| "IORegistry 中没有 AppleSMC 服务".to_string())?;
        let mut conn: io_connect_t = 0;
        // SAFETY: svc 是有效 IOService;mach_task_self() 是本任务端口;
        // type 0 是 SMC 约定的连接类型;conn 出参由内核填写。
        let kr = unsafe { IOServiceOpen(svc.raw(), mach_task_self(), 0, &mut conn) };
        if kr != 0 {
            return Err(format!("IOServiceOpen(AppleSMC) 失败,kr={kr}"));
        }
        Ok(Smc { conn, svc })
    }

    /// 执行一次 SMC 事务(data8 为命令码)。
    fn call(&self, input: &SmcKeyData) -> Option<SmcKeyData> {
        let mut output = SmcKeyData::default();
        let mut out_size = std::mem::size_of::<SmcKeyData>();
        // SAFETY: input/output 都是 80 字节定长结构;out_size 声明输出容量,
        // 内核按 SMC 协议只写该结构。conn 是 open() 拿到的有效连接。
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC,
                input as *const SmcKeyData as *const _,
                std::mem::size_of::<SmcKeyData>(),
                &mut output as *mut SmcKeyData as *mut _,
                &mut out_size,
            )
        };
        if kr != 0 {
            return None;
        }
        Some(output)
    }

    /// 查询键的存在性与元信息(`data_size` + `data_type`)。
    pub fn key_info(&self, key: &[u8; 4]) -> Option<(u32, u32)> {
        let mut input = SmcKeyData::default();
        input.key = u32::from_be_bytes(*key);
        input.data8 = SMC_CMD_READ_KEYINFO;
        let out = self.call(&input)?;
        if out.result != 0
            || out.key_info.data_size == 0
            || out.key_info.data_size > SMC_MAX_DATA as u32
        {
            return None;
        }
        Some((out.key_info.data_size, out.key_info.data_type))
    }

    /// 读键的原始字节(先查元信息,再取数据)。
    pub fn read_raw(&self, key: &[u8; 4]) -> Option<(u32, Vec<u8>)> {
        let (data_size, data_type) = self.key_info(key)?;
        let mut input = SmcKeyData::default();
        input.key = u32::from_be_bytes(*key);
        input.data8 = SMC_CMD_READ_BYTES;
        input.key_info.data_size = data_size;
        let out = self.call(&input)?;
        if out.result != 0 {
            return None;
        }
        Some((data_type, out.bytes[..data_size as usize].to_vec()))
    }

    /// 读并按类型解码。
    pub fn read(&self, key: &[u8; 4]) -> Option<SmcVal> {
        let (data_type, bytes) = self.read_raw(key)?;
        decode(data_type, &bytes)
    }

    /// 读数值(温度/风扇/功率等);类型不是数值时返回 None。
    pub fn read_f64(&self, key: &[u8; 4]) -> Option<f64> {
        self.read(key).and_then(|v| v.as_f64())
    }

    /// 按索引枚举键名(诊断用:跑 `dbg_mac_smc` 探针核对机型键位表)。
    pub fn key_at_index(&self, index: u32) -> Option<[u8; 4]> {
        let mut input = SmcKeyData::default();
        input.data8 = SMC_CMD_READ_INDEX;
        input.data32 = index;
        let out = self.call(&input)?;
        if out.result != 0 {
            return None;
        }
        Some(out.key.to_be_bytes())
    }

    /// 键总数(`#KEY` 键,ui32)。
    pub fn key_count(&self) -> Option<u32> {
        match self.read(b"#KEY")? {
            SmcVal::U64(v) => Some(v as u32),
            SmcVal::I64(v) => Some(v as u32),
            _ => None,
        }
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        // SAFETY: conn 由 IOServiceOpen 返回,只关闭一次;svc 的释放由 IoObject 负责。
        unsafe { IOServiceClose(self.conn) };
        // svc 不显式用,但要活到连接关闭之后
        let _ = &self.svc;
    }
}

/// 进程内共享的 SMC 连接(惰性打开)。轮询是单线程,锁实际无竞争;
/// 锁中毒时按项目惯例 `into_inner()` 继续用(数据仍然一致)。
static SHARED: std::sync::Mutex<Option<Smc>> = std::sync::Mutex::new(None);

/// 借共享 SMC 连接执行一次读取。SMC 不可用(虚拟机等)返回 None。
pub fn shared<T>(f: impl FnOnce(&Smc) -> Option<T>) -> Option<T> {
    let mut guard = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Smc::open().ok();
    }
    f(guard.as_ref()?)
}

/// 便捷读数值键(类型按运行时 key_info 解码)。
pub fn read_key(name: &str) -> Option<SmcVal> {
    let b = name.as_bytes();
    if b.len() != 4 {
        return None;
    }
    let mut key = [0u8; 4];
    key.copy_from_slice(b);
    shared(|smc| smc.read(&key))
}

/// 便捷读数值(浮点视图)。
pub fn read_f64(name: &str) -> Option<f64> {
    read_key(name).and_then(|v| v.as_f64())
}

/// 键是否存在于本机型 SMC(不读值)。
pub fn key_exists(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() != 4 {
        return false;
    }
    let mut key = [0u8; 4];
    key.copy_from_slice(b);
    shared(|smc| smc.key_info(&key)).is_some()
}

/// 按 4 字节类型码解码 SMC 原始字节(公开以便探针/测试直接核对换算)。
pub fn decode(data_type: u32, bytes: &[u8]) -> Option<SmcVal> {
    let t = data_type.to_be_bytes();
    match &t {
        b"sp78" => {
            let raw = i16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]);
            Some(SmcVal::F64(f64::from(raw) / 256.0))
        }
        b"fp88" => {
            let raw = u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]);
            Some(SmcVal::F64(f64::from(raw) / 256.0))
        }
        b"fpe2" => {
            let raw = u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]);
            Some(SmcVal::F64(f64::from(raw) / 4.0))
        }
        b"flt " => {
            let b = bytes.get(0..4)?;
            Some(SmcVal::F64(
                f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64
            ))
        }
        b"flag" => Some(SmcVal::Bool(*bytes.first()? != 0)),
        b"ch8 " => {
            let s: Vec<u8> = bytes.iter().copied().filter(|b| *b != 0).collect();
            Some(SmcVal::Str(String::from_utf8_lossy(&s).into_owned()))
        }
        b"ui8 " => Some(SmcVal::U64(*bytes.first()? as u64)),
        b"ui16" => {
            let b = bytes.get(0..2)?;
            Some(SmcVal::U64(u16::from_be_bytes([b[0], b[1]]) as u64))
        }
        b"ui32" => {
            let b = bytes.get(0..4)?;
            Some(SmcVal::U64(
                u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64
            ))
        }
        b"i8  " => Some(SmcVal::I64(*bytes.first()? as i8 as i64)),
        b"i16 " => {
            let b = bytes.get(0..2)?;
            Some(SmcVal::I64(i16::from_be_bytes([b[0], b[1]]) as i64))
        }
        b"i32 " => {
            let b = bytes.get(0..4)?;
            Some(SmcVal::I64(
                i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64
            ))
        }
        _ => Some(SmcVal::Bytes(bytes.to_vec())),
    }
}
