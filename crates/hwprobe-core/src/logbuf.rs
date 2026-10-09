//! 进程内环形日志缓冲:DLL 场景不能随便写文件,宿主程序通过
//! `hwprobe_last_error` / 未来扩展读取诊断信息。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const CAP: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

pub struct RingLog {
    entries: Mutex<VecDeque<(SystemTime, Level, String)>>,
    last_error: Mutex<Option<String>>,
}

impl RingLog {
    pub fn new() -> RingLog {
        RingLog {
            entries: Mutex::new(VecDeque::with_capacity(CAP)),
            last_error: Mutex::new(None),
        }
    }

    pub fn push(&self, level: Level, msg: impl Into<String>) {
        let msg = msg.into();
        if level == Level::Error {
            *self.last_error.lock().unwrap() = Some(msg.clone());
        }
        let mut q = self.entries.lock().unwrap();
        if q.len() >= CAP {
            q.pop_front();
        }
        q.push_back((SystemTime::now(), level, msg));
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    pub fn snapshot(&self) -> Vec<(SystemTime, Level, String)> {
        self.entries.lock().unwrap().iter().cloned().collect()
    }
}

static GLOBAL_LOG: OnceLock<RingLog> = OnceLock::new();

pub fn global() -> &'static RingLog {
    GLOBAL_LOG.get_or_init(RingLog::new)
}

pub fn info(msg: impl Into<String>) {
    global().push(Level::Info, msg);
}

pub fn warn(msg: impl Into<String>) {
    global().push(Level::Warn, msg);
}

pub fn error(msg: impl Into<String>) {
    global().push(Level::Error, msg);
}
