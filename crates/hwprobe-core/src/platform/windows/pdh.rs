//! PDH(性能计数器)助手。
//!
//! 统一封装:English 计数器路径(规避系统语言本地化问题)、通配符展开
//! (PdhAddCounter 不接受 `(*)`,须先展开为具体实例再逐个添加)、
//! 多实例格式化读取。CPU % Processor Utility 与 GPU Engine 利用率走这里。

#![allow(clippy::upper_case_acronyms)]
#![cfg(target_os = "windows")]

use windows::core::PWSTR;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhExpandCounterPathW,
    PdhGetFormattedCounterArrayW, PdhGetFormattedCounterValue, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

pub struct PdhQuery {
    handle: PDH_HQUERY,
}

unsafe impl Send for PdhQuery {}

impl PdhQuery {
    pub fn new() -> Result<PdhQuery, String> {
        let mut hq = PDH_HQUERY::default();
        let status = unsafe { PdhOpenQueryW(None, 0, &mut hq) };
        if status != 0 {
            return Err(format!("PdhOpenQuery 失败: 0x{status:08X}"));
        }
        Ok(PdhQuery { handle: hq })
    }

    /// 添加计数器,优先 English 直加;失败时通过性能计数器索引
    /// 翻译为本地化路径再添加(中文等本地化系统上 AddEnglishCounter
    /// 可能返回 PDH_CSTATUS_NO_OBJECT)。
    pub fn add_english(&mut self, path: &str) -> Result<PdhCounter, String> {
        match self.try_add_english(path) {
            Ok(c) => Ok(c),
            Err(en_err) => match translate_path(path) {
                Some(local) => self.add(&local),
                None => Err(en_err),
            },
        }
    }

    /// 添加(已本地化的)具体实例路径计数器。不接受通配符,
    /// 通配路径须先经 [`expand_english`] 展开。
    pub fn add(&mut self, path: &str) -> Result<PdhCounter, String> {
        use windows::Win32::System::Performance::PdhAddCounterW;
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hc = PDH_HCOUNTER::default();
        let status =
            unsafe { PdhAddCounterW(self.handle, windows::core::PCWSTR(wide.as_ptr()), 0, &mut hc) };
        if status != 0 {
            return Err(format!("PdhAddCounter({path}) 失败: 0x{status:08X}"));
        }
        Ok(PdhCounter { handle: hc })
    }

    fn try_add_english(&mut self, path: &str) -> Result<PdhCounter, String> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hc = PDH_HCOUNTER::default();
        let status =
            unsafe { PdhAddEnglishCounterW(self.handle, windows::core::PCWSTR(wide.as_ptr()), 0, &mut hc) };
        if status != 0 {
            return Err(format!("PdhAddEnglishCounter({path}) 失败: 0x{status:08X}"));
        }
        Ok(PdhCounter { handle: hc })
    }

    pub fn collect(&self) -> Result<(), String> {
        let status = unsafe { PdhCollectQueryData(self.handle) };
        if status != 0 {
            return Err(format!("PdhCollectQueryData 失败: 0x{status:08X}"));
        }
        Ok(())
    }
}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.handle);
        }
    }
}

#[derive(Clone, Copy)]
pub struct PdhCounter {
    handle: PDH_HCOUNTER,
}

unsafe impl Send for PdhCounter {}
unsafe impl Sync for PdhCounter {}

impl PdhCounter {
    /// 通配符计数器:返回全部实例的 (实例名, double 值)。
    /// 处理 PDH_MORE_DATA 重试(实例数在两次调用间可能变化)与缓冲区对齐。
    pub fn array_f64(&self) -> Result<Vec<(String, f64)>, String> {
        const PDH_MORE_DATA: u32 = 0x8000_07D2;
        unsafe {
            let mut size: u32 = 0;
            let mut count: u32 = 0;
            let mut status =
                PdhGetFormattedCounterArrayW(self.handle, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if status != PDH_MORE_DATA && status != 0 {
                return Err(format!("PdhGetFormattedCounterArray(size) 失败: 0x{status:08X}"));
            }
            // 重试:实例集合在请求缓冲区尺寸后可能再变化
            let mut buf: Vec<usize> = Vec::new();
            for _ in 0..4 {
                if size == 0 {
                    return Ok(Vec::new());
                }
                // usize 元素保证 8 字节对齐(结构体内含指针)
                buf = vec![0usize; size as usize / 8 + 1];
                let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
                count = 0;
                status = PdhGetFormattedCounterArrayW(
                    self.handle,
                    PDH_FMT_DOUBLE,
                    &mut size,
                    &mut count,
                    Some(items),
                );
                if status == 0 {
                    let mut out = Vec::with_capacity(count as usize);
                    for i in 0..count as usize {
                        let item = &*items.add(i);
                        // 跳过 CStatus 非 OK 的项(首采样的速率计数器等)
                        if item.FmtValue.CStatus != 0 {
                            continue;
                        }
                        let name = item.szName.to_string().unwrap_or_default();
                        let v = item.FmtValue.Anonymous.doubleValue;
                        if v.is_finite() {
                            out.push((name, v));
                        }
                    }
                    return Ok(out);
                }
                if status != PDH_MORE_DATA {
                    return Err(format!("PdhGetFormattedCounterArray 失败: 0x{status:08X}"));
                }
            }
            Err("PdhGetFormattedCounterArray: 实例数持续变化,重试耗尽".into())
        }
    }

    /// 单实例计数器读 double 值。
    pub fn value_f64(&self) -> Result<f64, String> {
        unsafe {
            let mut value = Default::default();
            let status = windows::Win32::System::Performance::PdhGetFormattedCounterValue(
                self.handle,
                PDH_FMT_DOUBLE,
                None,
                &mut value,
            );
            if status != 0 {
                return Err(format!("PdhGetFormattedCounterValue 失败: 0x{status:08X}"));
            }
            if value.CStatus != 0 {
                return Err(format!("计数器 CStatus={}", value.CStatus));
            }
            Ok(value.Anonymous.doubleValue)
        }
    }
}

/// 从 PDH 实例名中提取前导整数(如 "0 C:" → 0,"0,1" → 1)。
pub fn leading_index(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// 取计数器路径中括号内的实例名:"\\Obj(0,3)\\Cnt" → "0,3"。
pub fn path_instance(path: &str) -> Option<&str> {
    let a = path.find('(')? + 1;
    let b = path[a..].find(')')? + a;
    Some(&path[a..b])
}

/// 展开通配符计数器路径为具体实例路径列表。
///
/// `PdhAddCounter`/`PdhAddEnglishCounter` 均不接受 `(*)` 通配符
/// (直接添加返回 0xC0000BC0),必须先展开再逐个添加。
/// 输入为 English 形式 `"Object(*)\\Counter"`;本地化系统先翻译成
/// 本地化路径再展开(实例名本身不本地化,如 "0,3"、pid_xxx)。
/// 返回形如 `"\\Object(instance)\\Counter"` 的完整列表。
pub fn expand_english(path: &str) -> Result<Vec<String>, String> {
    // 本地化系统上 English 名匹配不到,先翻译;翻译失败按原样尝试(英文系统)
    let full = format!("\\{}", translate_path(path).unwrap_or_else(|| path.to_string()));
    let wide = wide0(&full);
    let mut chars = 0u32;
    let mut status = unsafe {
        PdhExpandCounterPathW(windows::core::PCWSTR(wide.as_ptr()), None, &mut chars)
    };
    const PDH_MORE_DATA: u32 = 0x8000_07D2;
    if status != 0 && status != PDH_MORE_DATA {
        return Err(format!("PdhExpandCounterPath(size) {path} 失败: 0x{status:08X}"));
    }
    if chars == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u16; chars as usize];
    let status = unsafe {
        PdhExpandCounterPathW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(PWSTR(buf.as_mut_ptr())),
            &mut chars,
        )
    };
    if status != 0 {
        return Err(format!("PdhExpandCounterPath({path}) 失败: 0x{status:08X}"));
    }
    // MULTI_SZ:条目间单 NUL 结尾,整体双 NUL 结束
    Ok(buf
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf16_lossy(s))
        .collect())
}

/// 把 "Object(*)\Counter" 形式的 English 路径翻译为本地化路径。
/// 通过 PdhLookupPerfIndexByEnglishName → PdhLookupPerfNameByIndex 完成。
fn translate_path(path: &str) -> Option<String> {
    use windows::Win32::System::Performance::{
        PdhLookupPerfIndexByNameW, PdhLookupPerfNameByIndexW,
    };

    fn lookup_index(english: &str) -> Option<u32> {
        let wide = wide0(english);
        let mut idx = 0u32;
        let status = unsafe {
            PdhLookupPerfIndexByNameW(
                windows::core::PCWSTR::null(),
                windows::core::PCWSTR(wide.as_ptr()),
                &mut idx,
            )
        };
        (status == 0).then_some(idx)
    }

    fn lookup_name(index: u32) -> Option<String> {
        unsafe {
            let mut chars = 0u32;
            // 输入 0 长度会返回 PDH_MORE_DATA 并给出所需字符数
            let mut status = PdhLookupPerfNameByIndexW(None, index, None, &mut chars);
            if status != 0 && status != 0x8000_07D2 {
                return None;
            }
            if chars == 0 {
                return None;
            }
            let mut buf = vec![0u16; chars as usize];
            status = PdhLookupPerfNameByIndexW(
                None,
                index,
                Some(windows::core::PWSTR(buf.as_mut_ptr())),
                &mut chars,
            );
            if status != 0 {
                return None;
            }
            // 末尾可能带一个 NUL
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        }
    }

    let (object, counter) = path.split_once('\\')?;
    let object = object.trim_end_matches("(*)").trim_end_matches('(').trim();
    let obj_local = lookup_index(object).and_then(lookup_name)?;
    let cnt_local = lookup_index(counter).and_then(lookup_name)?;
    Some(format!("{obj_local}(*)\\{cnt_local}"))
}

fn wide0(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[allow(dead_code)]
fn _unused(_: PWSTR) {}
