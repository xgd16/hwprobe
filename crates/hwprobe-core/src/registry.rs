//! SensorRegistry:provider 集合 + 后台轮询线程 + TTL 缓存 + 对外读值接口。
//!
//! 并发模型:
//! - 每个 provider 独占一把 RwLock,写锁只在 provider.refresh() 期间持有;
//! - 后台轮询线程是唯一的写者(除显式 force refresh),读操作只拿读锁;
//! - 读操作最多等一次 refresh 的时长(动态刷新全部走轻量原生 API,毫秒级),
//!   WMI 等慢源只允许出现在 probe(初始化)阶段。

use crate::logbuf;
use crate::model::{
    Category, DeviceData, Reading, SensorData, SensorKind, Status, ValueType, ALL_CATEGORIES,
    CATEGORY_COUNT,
};
use crate::stream::{StreamBatch, StreamCallback, StreamItem, SubscriptionHub};
use crate::traits::Provider;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct SlotState {
    devices: Vec<DeviceData>,
    last_refresh: Option<Instant>,
    /// 累计刷新次数。差分类传感器(磁盘/网络吞吐、CPU/GPU 占用)要有两个
    /// 采样点才算得出值;`refreshes < 2` 时刷新门槛降到「一个节拍」而不是
    /// provider TTL,让第二个采样点尽早到来 —— 不让调用方凭空等满 TTL,
    /// 也不把首个读数的窗口压到一次循环迭代那么短。
    refreshes: u32,
    probe_error: Option<String>,
}

impl SlotState {
    /// 距上次刷新是否已到本次的门槛:还没有第二个采样点(`refreshes < 2`)
    /// 时用节拍 `prime`,之后用 provider 的 `ttl`。
    fn refresh_due(&self, last: Instant, ttl: Duration, prime: Duration) -> bool {
        last.elapsed() >= if self.refreshes < 2 { prime } else { ttl }
    }
}

struct Slot {
    provider: Arc<dyn Provider>,
    state: RwLock<SlotState>,
}

pub struct Registry {
    slots: Vec<Slot>,
    stop: Arc<AtomicBool>,
    poller: RwLock<Option<JoinHandle<()>>>,
    /// 轮询节拍(ms),默认 500ms 扫描一轮,到 TTL 的 provider 才真正刷新。
    tick_ms: AtomicU32,
    /// 订阅式推送(见 crate::stream)。
    hub: SubscriptionHub,
    /// 推送批次号。
    seq: AtomicU64,
}

impl Registry {
    /// 用一组 provider 构建注册表并立即执行 probe。
    /// 单个 provider probe 失败:记录日志,该类别保留 0 设备,不阻断整体。
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Registry {
        let mut slots = Vec::with_capacity(providers.len());
        for provider in providers {
            let cat = provider.category();
            let (devices, err) = match provider.probe() {
                Ok(devs) => (devs, None),
                Err(e) => {
                    logbuf::warn(format!(
                        "provider {} ({}): probe 失败: {}",
                        cat.name(),
                        provider.describe(),
                        e
                    ));
                    (Vec::new(), Some(e))
                }
            };
            logbuf::info(format!(
                "provider {}: {} 个设备就绪",
                cat.name(),
                devices.len()
            ));
            slots.push(Slot {
                provider,
                state: RwLock::new(SlotState {
                    devices,
                    last_refresh: None,
                    refreshes: 0,
                    probe_error: err,
                }),
            });
        }
        Registry {
            slots,
            stop: Arc::new(AtomicBool::new(false)),
            poller: RwLock::new(None),
            tick_ms: AtomicU32::new(500),
            hub: SubscriptionHub::new(),
            seq: AtomicU64::new(0),
        }
    }

    /// 启动后台轮询线程(需要 Arc<Registry>,线程持有 Arc 保证存活)。
    /// 重复调用无副作用。
    pub fn spawn_poller(self: &Arc<Self>) {
        let mut guard = self.poller.write().unwrap();
        if guard.is_some() {
            return;
        }
        self.stop.store(false, Ordering::SeqCst);
        let stop = self.stop.clone();
        let this = self.clone();
        let handle = std::thread::Builder::new()
            .name("hwprobe-poller".into())
            .spawn(move || {
                // 轮询线程会触碰 DXGI 等 COM 对象:确保本线程处于 COM MTA。
                // 已初始化(STA/S_FALSE)时忽略错误,依赖对象自由线程模型。
                #[cfg(target_os = "windows")]
                unsafe {
                    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let mut last_dispatch = Instant::now() - Duration::from_secs(1);
                while !stop.load(Ordering::SeqCst) {
                    this.tick();
                    let ms = this.tick_ms.load(Ordering::Relaxed) as u64;
                    // 订阅推送:按当前节拍检查到期订阅并派发快照。
                    // (有订阅要求更快间隔时 tick_ms 已被调低)
                    if last_dispatch.elapsed().as_millis() as u64 >= ms {
                        this.dispatch_streams();
                        last_dispatch = Instant::now();
                    }
                    // 小步睡眠,保证 stop 与 set_poll_interval 快速生效
                    std::thread::sleep(Duration::from_millis(ms.min(200)));
                }
                logbuf::info("poller 线程退出");
            })
            .ok();
        *guard = handle;
    }

    pub fn stop_poller(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let handle = self.poller.write().unwrap().take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    pub fn set_poll_interval(&self, ms: u32) {
        let ms = ms.clamp(100, 60_000);
        self.tick_ms.store(ms, Ordering::Relaxed);
    }

    /// 订阅动态数据流(推送模型,见 [`crate::stream`])。
    /// `interval_ms` 为期望的推送间隔,0 表示跟随后台节拍每次都推;
    /// 非零值会把后台节拍自动调低到不超过该间隔(下限 100ms)。
    /// 返回订阅句柄,用于 [`Registry::unsubscribe`]。
    pub fn subscribe(&self, interval_ms: u32, category_mask: u32, cb: StreamCallback) -> u64 {
        let effective = if interval_ms == 0 {
            0
        } else {
            interval_ms.clamp(100, 60_000)
        };
        if effective > 0 && effective < self.tick_ms.load(Ordering::Relaxed) {
            self.tick_ms.store(effective, Ordering::Relaxed);
        }
        self.hub.subscribe(effective, category_mask, cb)
    }

    /// 取消订阅。句柄不存在返回 [`Status::BadHandle`]。
    pub fn unsubscribe(&self, handle: u64) -> Result<(), Status> {
        self.hub.unsubscribe(handle)
    }

    /// 派发一轮推送:没有到期订阅时不构建快照,零开销。
    fn dispatch_streams(&self) {
        let now = Instant::now();
        if !self.hub.any_due(now) {
            return;
        }
        let batch = self.build_batch();
        self.hub.dispatch(&batch, now);
    }

    /// 构建一批快照:全部动态传感器的当前读值,按 类别→设备→传感器 展平。
    fn build_batch(&self) -> StreamBatch {
        let mut items = Vec::new();
        for slot in &self.slots {
            let cat = slot.provider.category() as u32;
            let st = slot.state.read().unwrap_or_else(|e| e.into_inner());
            for (di, dev) in st.devices.iter().enumerate() {
                for (si, sen) in dev.sensors.iter().enumerate() {
                    if sen.desc.kind != SensorKind::Dynamic {
                        continue;
                    }
                    items.push(StreamItem {
                        category: cat,
                        device: di as u32,
                        sensor: si as u32,
                        value_type: sen.desc.vtype as u32,
                        status: sen.current.status as u32,
                        timestamp_ms: sen.current.timestamp_ms,
                        value: sen.current.value.clone(),
                    });
                }
            }
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        StreamBatch {
            seq,
            timestamp_ms: crate::util::now_ms(),
            items,
        }
    }

    /// 轮询一轮:把到 TTL 的 provider 依次刷新。每个 provider 独立持锁,
    /// 单个失败被捕获并记录,不影响其余。
    fn tick(&self) {
        // 第二个采样点的门槛:取「一个节拍」与「provider TTL」的较小者。
        // 差分读数要两个采样点才算得出值,不能等满 TTL(磁盘/CPU/GPU/网卡
        // 都是 1000ms);但也不能在下一个循环迭代(几百微秒级滑步,上限
        // 200ms)就取,那样首个速率的窗口过短,突发型 I/O 下会明显高于
        // 持续速率。用节拍当门槛,首值窗口与后续采样窗口同量级。
        let prime_gate = Duration::from_millis(
            self.tick_ms.load(Ordering::Relaxed).clamp(100, 60_000) as u64,
        );
        for slot in &self.slots {
            let ttl = slot.provider.ttl();
            let due = {
                let st = slot.state.read().unwrap();
                match st.last_refresh {
                    Some(t) => st.refresh_due(t, ttl, prime_gate),
                    None => true,
                }
            };
            if !due {
                continue;
            }
            let mut st = slot.state.write().unwrap();
            // 二次检查(可能已被 force refresh 更新)
            if let Some(t) = st.last_refresh {
                if !st.refresh_due(t, ttl, prime_gate) {
                    continue;
                }
            }
            let devs = &mut st.devices;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| slot.provider.refresh(devs)));
            if result.is_err() {
                logbuf::error(format!(
                    "provider {} refresh panic(已捕获,保留旧数据)",
                    slot.provider.category().name()
                ));
            }
            st.refreshes = st.refreshes.saturating_add(1);
            st.last_refresh = Some(Instant::now());
        }
    }

    /// 立即刷新一个类别(None = 全部),绕过 TTL。
    pub fn refresh(&self, category: Option<Category>) {
        for slot in &self.slots {
            if let Some(c) = category {
                if slot.provider.category() != c {
                    continue;
                }
            }
            let mut st = slot.state.write().unwrap();
            let devs = &mut st.devices;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| slot.provider.refresh(devs)));
            if result.is_err() {
                logbuf::error(format!(
                    "provider {} refresh panic(已捕获)",
                    slot.provider.category().name()
                ));
            }
            st.refreshes = st.refreshes.saturating_add(1);
            st.last_refresh = Some(Instant::now());
        }
    }

    /// 范围内是否每个 provider 都已完成至少两次刷新 —— 即差分类传感器
    /// (磁盘/网络吞吐、CPU/GPU 占用)的第二个采样点已经拿到,第一个读数
    /// 不会再是「等待首刷」。静态-only 的类别同样计入(刷新本身是空操作)。
    pub fn ready(&self, category: Option<Category>) -> bool {
        self.slots.iter().all(|s| {
            if let Some(c) = category {
                if s.provider.category() != c {
                    return true;
                }
            }
            s.state.read().unwrap().refreshes >= 2
        })
    }

    /// 等待 `ready()`,最多 timeout。返回是否按时就绪;超时由调用方决定
    /// 如何呈现(工具侧照常打印,状态仍是「等待首刷」,不假装有数据)。
    pub fn wait_ready(&self, category: Option<Category>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ready(category) {
                return true;
            }
            if Instant::now() >= deadline {
                return self.ready(category);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn slot_by_category(&self, category: u32) -> Option<&Slot> {
        let cat = Category::from_u32(category)?;
        self.slots.iter().find(|s| s.provider.category() == cat)
    }

    pub fn device_count(&self, category: u32) -> Result<u32, Status> {
        let slot = self.slot_by_category(category).ok_or(Status::BadCategory)?;
        let st = slot.state.read().unwrap();
        Ok(st.devices.len() as u32)
    }

    pub fn device(&self, category: u32, index: u32) -> Result<DeviceData, Status> {
        let slot = self.slot_by_category(category).ok_or(Status::BadCategory)?;
        let st = slot.state.read().unwrap();
        st.devices
            .get(index as usize)
            .cloned()
            .ok_or(Status::BadDevice)
    }

    pub fn sensor_count(&self, category: u32, device: u32) -> Result<u32, Status> {
        self.device(category, device)
            .map(|d| d.sensors.len() as u32)
    }

    pub fn read(
        &self,
        category: u32,
        device: u32,
        sensor: u32,
    ) -> Result<Reading, Status> {
        let slot = self.slot_by_category(category).ok_or(Status::BadCategory)?;
        let st = slot.state.read().unwrap();
        let dev = st.devices.get(device as usize).ok_or(Status::BadDevice)?;
        let sen = dev.sensors.get(sensor as usize).ok_or(Status::BadSensor)?;
        Ok(sen.current.clone())
    }

    pub fn category_present(&self, category: Category) -> bool {
        self.slots
            .iter()
            .any(|s| s.provider.category() == category)
    }

    pub fn dump_json(&self, category: u32) -> serde_json::Value {
        let cats: Vec<serde_json::Value> = Category::ALL
            .iter()
            .filter(|c| category == ALL_CATEGORIES || **c as u32 == category)
            .map(|c| {
                let slot = self.slots.iter().find(|s| s.provider.category() == *c);
                match slot {
                    None => json!({
                        "index": *c as u32, "name": c.name(),
                        "available": false,
                    }),
                    Some(slot) => {
                        let st = slot.state.read().unwrap();
                        let devices: Vec<serde_json::Value> = st
                            .devices
                            .iter()
                            .map(device_to_json)
                            .collect();
                        let mut v = json!({
                            "index": *c as u32,
                            "name": c.name(),
                            "available": true,
                            "probe_error": st.probe_error,
                            "devices": devices,
                        });
                        if st.probe_error.is_none() {
                            v.as_object_mut().unwrap().remove("probe_error");
                        }
                        v
                    }
                }
            })
            .collect();
        json!({ "categories": cats })
    }
}

fn device_to_json(dev: &DeviceData) -> serde_json::Value {
    let sensors: Vec<serde_json::Value> = dev
        .sensors
        .iter()
        .map(|s: &SensorData| {
            json!({
                "code": s.desc.code,
                "name": s.desc.name,
                "unit": s.desc.unit,
                "kind": s.desc.kind as u32,
                "value_type": s.desc.vtype as u32,
                "status": s.current.status as u32,
                "value": s.current.value.to_json(),
                "timestamp_ms": s.current.timestamp_ms,
            })
        })
        .collect();
    json!({
        "id": dev.meta.id,
        "name": dev.meta.name,
        "vendor": dev.meta.vendor,
        "model": dev.meta.model,
        "serial": dev.meta.serial,
        "sensors": sensors,
    })
}

/// 供 dump 层使用:ValueType 的 C 常量值。
pub fn value_type_tag(vt: ValueType) -> u32 {
    vt as u32
}

pub const _CATEGORY_COUNT_CHECK: u32 = CATEGORY_COUNT;

/// 在专职工作线程上创建后端并完成 probe。
///
/// 两个原因必须用常驻工作线程:
/// 1. WMI 的 COM 初始化要求 MTA;宿主进程(如 PowerShell/C# WinForms)
///    的调用线程往往是 STA,直接在其上 probe 会导致 WMI 类 provider 全部失败;
/// 2. probe 创建的 DXGI 等 COM 对象归属于初始化线程所在的 MTA ——
///    线程退出会隐式 CoUninitialize 并销毁 MTA,后续任何线程再调用
///    这些对象都会访问违例。因此 worker 线程在 probe 后驻留(持有 MTA)。
pub fn build_on_worker() -> Result<(Box<dyn crate::traits::PlatformBackend>, Arc<Registry>), String>
{
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hwprobe-init".into())
        .spawn(move || {
            let result = (|| {
                let mut backend = crate::create_backend()?;
                if let Err(e) = backend.init() {
                    logbuf::warn(format!("平台增强层初始化失败(相关传感器将标记 NO_DRIVER): {e}"));
                }
                let providers = backend.providers();
                let registry = Arc::new(Registry::new(providers));
                Ok((backend, registry))
            })();
            let ok = tx.send(result).is_ok();
            // 驻留线程以保活本线程上的 COM MTA(见函数注释)。
            if ok {
                loop {
                    std::thread::park();
                    // park 返回只有两种:永久阻塞前被解除(继续 park)
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                }
            }
        });
    if spawned.is_err() {
        return Err("无法创建初始化线程".into());
    }
    match rx.recv() {
        Ok(res) => res,
        Err(_) => Err("初始化线程意外退出".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceBuilder;
    use std::sync::atomic::AtomicU32;

    /// TTL 远大于轮询节拍的 provider:用于验证「第二个采样点不被 TTL 挡住」。
    struct Stub {
        refreshes: AtomicU32,
    }

    impl Provider for Stub {
        fn describe(&self) -> &str {
            "stub"
        }
        fn category(&self) -> Category {
            Category::Disk
        }
        fn probe(&self) -> Result<Vec<DeviceData>, String> {
            Ok(vec![DeviceBuilder::new("stub.pd0", "Stub Disk")
                .dynamic("rate", "Rate", "KB/s", ValueType::F64)
                .build()])
        }
        fn ttl(&self) -> Duration {
            Duration::from_secs(60)
        }
        fn refresh(&self, devices: &mut [DeviceData]) {
            let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            devices[0].set_reading("rate", Reading::now(crate::model::Value::F64(n as f64)));
        }
    }

    fn stub_registry() -> (Arc<Registry>, Arc<Stub>) {
        let stub = Arc::new(Stub {
            refreshes: AtomicU32::new(0),
        });
        let reg = Arc::new(Registry::new(vec![stub.clone() as Arc<dyn Provider>]));
        reg.set_poll_interval(100);
        reg.spawn_poller();
        (reg, stub)
    }

    /// ttl = 60s 的 provider 也必须在几百毫秒内拿到第二个采样点,
    /// 否则差分类读数(磁盘/网络吞吐等)要凭空等满一个 TTL 才有值。
    #[test]
    fn second_sample_is_not_gated_by_ttl() {
        let (reg, stub) = stub_registry();
        assert!(
            reg.wait_ready(Some(Category::Disk), Duration::from_millis(2000)),
            "第二个采样点被 TTL 挡住了(刷新 {} 次)",
            stub.refreshes.load(Ordering::SeqCst)
        );
        assert!(reg.ready(None), "ready() 应与 wait_ready 一致");
        assert!(stub.refreshes.load(Ordering::SeqCst) >= 2);
        reg.stop_poller();
    }

    /// 未启动轮询线程时不应就绪,超时后返回 false(调用方据此提示,而不是假装有数据)。
    #[test]
    fn wait_ready_times_out_without_poller() {
        let stub = Arc::new(Stub {
            refreshes: AtomicU32::new(0),
        });
        let reg = Registry::new(vec![stub as Arc<dyn Provider>]);
        assert!(!reg.ready(Some(Category::Disk)));
        assert!(!reg.wait_ready(Some(Category::Disk), Duration::from_millis(120)));
        // 只等指定类别:范围外的类别不该让它变慢
        assert!(reg.wait_ready(Some(Category::Gpu), Duration::from_millis(50)));
    }
}
