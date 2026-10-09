//! 订阅式流式推送。
//!
//! 调用方向注册表登记一个回调(订阅),后台轮询线程每轮刷新后把全部
//! 动态传感器快照(一个批次)推给到期的订阅者 —— 调用方无需再自行轮询
//! 读值/刷新,把"调用方高频拉取"的负载变成"DLL 按节拍推送"。
//!
//! 线程契约:
//! - 回调在内部轮询线程上执行,必须快速返回(建议只做数据拷贝/入队),
//!   不得在回调里阻塞,不得调用会刷新或关闭库的接口;
//! - [`SubscriptionHub::unsubscribe`] 保证返回之后该订阅的回调不会再被
//!   调用(等待在途回调结束);允许在回调内部退订自身;
//! - 回调 panic 会被捕获并记录,不影响轮询线程与其他订阅者。

use crate::logbuf;
use crate::model::{Status, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

/// 一条推送样本:某个 (category, device, sensor) 动态传感器的一次读值。
#[derive(Debug, Clone)]
pub struct StreamItem {
    pub category: u32,
    pub device: u32,
    pub sensor: u32,
    /// [`crate::model::ValueType`] 的 C 常量值。
    pub value_type: u32,
    /// [`crate::model::Status`] 的 C 常量值(单传感器粒度)。
    pub status: u32,
    pub timestamp_ms: u64,
    pub value: Value,
}

/// 一批推送样本:一轮刷新后的完整动态传感器快照。
#[derive(Debug, Clone)]
pub struct StreamBatch {
    /// 单调递增批次号(从 1 开始)。
    pub seq: u64,
    /// 批次构建时刻(Unix 毫秒)。
    pub timestamp_ms: u64,
    pub items: Vec<StreamItem>,
}

pub type StreamCallback = Arc<dyn Fn(&StreamBatch) + Send + Sync>;

struct Subscription {
    /// 两次推送的最小间隔;0 = 跟随后台节拍每次都推。
    interval: Duration,
    /// 类别掩码:bit i = 类别 i;全 1 表示全部。
    mask: u32,
    callback: StreamCallback,
    next_due: Mutex<Instant>,
    /// 回调执行期间由派发线程持有;unsubscribe 用它等待在途回调。
    invoking: Mutex<()>,
    /// 当前执行回调的线程,用于"回调内退订自身"的死锁规避。
    invoking_thread: Mutex<Option<ThreadId>>,
    /// 置 false 后派发线程不再进入该订阅。
    active: AtomicBool,
}

/// 订阅中心:管理订阅生命周期与批次派发。
pub struct SubscriptionHub {
    next_id: AtomicU64,
    subs: RwLock<HashMap<u64, Arc<Subscription>>>,
}

// 回调 panic 后 Mutex 可能带毒;带毒锁的内部数据仍然一致,直接取用。
fn lock_mutex<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn lock_rw<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

impl SubscriptionHub {
    pub fn new() -> SubscriptionHub {
        SubscriptionHub {
            next_id: AtomicU64::new(0),
            subs: RwLock::new(HashMap::new()),
        }
    }

    /// 登记订阅,返回句柄。首批数据在下一次派发时立即推送。
    pub fn subscribe(&self, interval_ms: u32, mask: u32, cb: StreamCallback) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut subs = self.subs.write().unwrap_or_else(|e| e.into_inner());
        subs.insert(
            id,
            Arc::new(Subscription {
                interval: Duration::from_millis(interval_ms as u64),
                mask,
                callback: cb,
                next_due: Mutex::new(Instant::now()),
                invoking: Mutex::new(()),
                invoking_thread: Mutex::new(None),
                active: AtomicBool::new(true),
            }),
        );
        id
    }

    /// 取消订阅。句柄不存在(或已取消)返回 [`Status::BadHandle`]。
    /// 返回后该订阅的回调保证不会再被调用。
    pub fn unsubscribe(&self, id: u64) -> Result<(), Status> {
        let sub = {
            let mut subs = self.subs.write().unwrap_or_else(|e| e.into_inner());
            subs.remove(&id).ok_or(Status::BadHandle)?
        };
        sub.active.store(false, Ordering::SeqCst);
        let current = thread::current().id();
        loop {
            match sub.invoking.try_lock() {
                Ok(_guard) => break, // 没有回调在执行
                Err(_) => {
                    if *lock_mutex(&sub.invoking_thread) == Some(current) {
                        break; // 回调内部退订自身
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
        Ok(())
    }

    /// 有订阅到期需要派发吗?没有订阅或都没到期时跳过快照构建。
    pub fn any_due(&self, now: Instant) -> bool {
        let subs = lock_rw(&self.subs);
        subs.values().any(|s| {
            s.active.load(Ordering::Relaxed) && *lock_mutex(&s.next_due) <= now
        })
    }

    /// 最小的非零订阅间隔,用于把后台节拍提速到最快订阅者的要求。
    pub fn min_interval_ms(&self) -> Option<u32> {
        let subs = lock_rw(&self.subs);
        subs.values()
            .filter(|s| s.active.load(Ordering::Relaxed))
            .map(|s| s.interval.as_millis() as u32)
            .filter(|ms| *ms > 0)
            .min()
    }

    /// 把一批快照推给所有到期订阅者。mask 不匹配的类别会被过滤;
    /// 单个回调 panic 被捕获记录,不影响其他订阅者。
    pub fn dispatch(&self, batch: &StreamBatch, now: Instant) {
        let snapshot: Vec<Arc<Subscription>> = {
            let subs = lock_rw(&self.subs);
            subs.values().cloned().collect()
        };
        for sub in snapshot {
            if !sub.active.load(Ordering::SeqCst) {
                continue;
            }
            {
                let mut due = lock_mutex(&sub.next_due);
                if *due > now {
                    continue;
                }
                *due = now + sub.interval;
            }
            let _guard = lock_mutex(&sub.invoking);
            // 二次检查:unsubscribe 可能在等锁期间已把订阅标记为取消
            if !sub.active.load(Ordering::SeqCst) {
                continue;
            }
            *lock_mutex(&sub.invoking_thread) = Some(thread::current().id());
            let result = if sub.mask == u32::MAX {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (sub.callback)(batch);
                }))
            } else {
                let items: Vec<StreamItem> = batch
                    .items
                    .iter()
                    .filter(|it| sub.mask & (1u32 << it.category) != 0)
                    .cloned()
                    .collect();
                let filtered = StreamBatch {
                    seq: batch.seq,
                    timestamp_ms: batch.timestamp_ms,
                    items,
                };
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (sub.callback)(&filtered);
                }))
            };
            *lock_mutex(&sub.invoking_thread) = None;
            if result.is_err() {
                logbuf::error("订阅回调 panic(已捕获,不影响后续推送)");
            }
        }
    }
}

impl Default for SubscriptionHub {
    fn default() -> Self {
        Self::new()
    }
}
