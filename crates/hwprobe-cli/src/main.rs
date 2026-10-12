//! hwprobe-cli:硬件信息 dump 与验证工具。
//!
//! 用法:
//!   hwprobe-cli dump [--json] [--category <名称|索引>] [--wait <ms>]
//!                    [--usage-mode <default|taskmgr>]
//!   hwprobe-cli watch [--interval <ms>] [--category <名称|索引>]
//!                     [--usage-mode <default|taskmgr>]
//!   hwprobe-cli stream [--interval <ms>] [--category <名称|索引>] [--count <n>] [--full]
//!                      [--usage-mode <default|taskmgr>]
//!   hwprobe-cli setup     一键安装 PawnIO 驱动增强层(UAC 提权 + 静默)
//!   hwprobe-cli log        输出内部诊断日志
//!   hwprobe-cli version
//!
//! --usage-mode:占用口径。default(默认)= 时间差分 / 厂商 API;
//! taskmgr = 类 Windows 任务管理器(PDH % Processor Utility / GPU Engine 计数器)。
//!
//! --wait:等待首个读数的**上限**(默认 2000ms),就绪即返回。差分类读数
//! (磁盘/网络吞吐、CPU/GPU 占用)必须有两个采样点才有值,所以判断依据是
//! 「范围内每个 provider 都刷过两次」,而不是固定睡满这段时间。

use hwprobe_core::model::{Category, SensorKind, Value};
use hwprobe_core::Registry;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("dump");
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let opt_after = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    match cmd {
        "version" => {
            println!("hwprobe-cli {}", env!("CARGO_PKG_VERSION"));
            println!("abi {}", hwprobe_core::HWPROBE_ABI_VERSION);
        }
        "log" => {
            for (t, level, msg) in hwprobe_core::logbuf::global().snapshot() {
                let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                println!("[{secs}] {:5} {}", level.as_str(), msg);
            }
        }
        "setup" => cmd_setup(),
        "dump" | "watch" => {
            let category = opt_after("--category").map(|s| resolve_category(&s)).filter(|_| true);
            apply_usage_mode(&opt_after("--usage-mode"));
            run(cmd == "watch", has("--json"), category, &opt_after("--wait"), &opt_after("--interval"), has("--log"));
        }
        "stream" => {
            let category = opt_after("--category").map(|s| resolve_category(&s)).filter(|_| true);
            apply_usage_mode(&opt_after("--usage-mode"));
            cmd_stream(&opt_after("--interval"), category, &opt_after("--count"), has("--full"));
        }
        other => {
            eprintln!("未知命令: {other}");
            eprintln!("用法: hwprobe-cli dump [--json] | watch | stream | setup | log | version");
            std::process::exit(2);
        }
    }
}

/// 一键安装 PawnIO 驱动增强层(内置官方安装器,UAC 提权 + 静默安装),
/// 随后验证驱动层在本进程是否可用。
///
/// 非 Windows 平台没有内核驱动增强层(macOS 传感器全部走用户态接口),
/// 该命令仅打印说明,不做任何事。
fn cmd_setup() {
    println!("== hwprobe 驱动增强层安装 ==");

    #[cfg(target_os = "windows")]
    {
        use hwprobe_core::platform::windows::driver;

        if driver::pawnio_installed() {
            println!("PawnIO:已安装,无需重复安装");
        } else {
            println!("PawnIO:未安装,即将运行内置官方安装器(弹出 UAC,请选择\"是\")…");
            if let Err(e) = driver::install_pawnio() {
                eprintln!("PawnIO:安装失败: {e}");
                eprintln!("也可手动安装: winget install namazso.PawnIO");
                std::process::exit(1);
            }
            println!("PawnIO:安装成功");
        }

        match driver::try_init() {
            Ok(_) => println!("驱动增强层:可用 ✓(CPU 温度/功耗、主板风扇/电压传感器已解锁)"),
            Err(_) => {
                eprintln!("驱动增强层:当前进程不可用 —— 访问 PawnIO 需要管理员权限,");
                eprintln!("请以管理员身份重新运行本命令或调用方程序(温度/风扇/电压方能出值)。");
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        println!("本平台没有内核驱动增强层 —— 传感器全部走用户态接口,无需安装任何驱动。");
        println!("(macOS 上部分传感器如 NVMe SMART 需要 root 权限,以 sudo 运行即可解锁。)");
    }
}

fn resolve_category(s: &str) -> u32 {
    if let Ok(i) = s.parse::<u32>() {
        return i;
    }
    for c in Category::ALL {
        if c.name().eq_ignore_ascii_case(s) {
            return c as u32;
        }
    }
    eprintln!("未知类别: {s}");
    std::process::exit(2);
}

/// 应用 --usage-mode(0/default/standard 或 1/taskmgr),必须在构建
/// registry 之前调用 —— 占用传感器首刷即按该口径出值。
fn apply_usage_mode(opt: &Option<String>) {
    let Some(s) = opt else { return };
    let mode = match s.to_ascii_lowercase().as_str() {
        "0" | "default" | "standard" => 0u32,
        "1" | "taskmgr" | "taskmanager" | "task-manager" => 1u32,
        other => {
            eprintln!("未知占用口径: {other}(可选 default | taskmgr)");
            std::process::exit(2);
        }
    };
    if !hwprobe_core::options::set_usage_mode(mode) {
        eprintln!("非法占用模式: {mode}");
        std::process::exit(2);
    }
    println!(
        "占用口径: {}",
        if mode == 1 { "类 Windows 任务管理器 (taskmgr)" } else { "标准 (default)" }
    );
}

fn build_registry() -> Arc<Registry> {
    // 在专职线程上创建 + probe(WMI 需要 MTA 环境)
    let (_backend, registry) = hwprobe_core::registry::build_on_worker().unwrap_or_else(|e| {
        eprintln!("初始化失败: {e}");
        std::process::exit(1);
    });
    registry.spawn_poller();
    registry
}

fn run(
    watch: bool,
    json: bool,
    category: Option<u32>,
    wait: &Option<String>,
    interval: &Option<String>,
    log: bool,
) {
    let registry = build_registry();
    let wait_ms: u64 = wait
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);
    // 就绪即返回,最迟等 wait_ms:差分类读数(磁盘/网络吞吐、CPU/GPU 占用)
    // 需要两个采样点,固定 sleep 会让「等了多久」和「多久才有值」错位。
    if !registry.wait_ready(
        category.and_then(Category::from_u32),
        Duration::from_millis(wait_ms),
    ) {
        eprintln!("注意: {wait_ms}ms 内仍有类别未取到第二个采样点,下列「等待首刷」项尚无值");
    }

    let print_log = || {
        if log {
            eprintln!("---- 内部诊断日志 ----");
            for (t, level, msg) in hwprobe_core::logbuf::global().snapshot() {
                let secs = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                eprintln!("[{secs}] {:5} {}", level.as_str(), msg);
            }
        }
    };

    if json {
        let v = registry.dump_json(category.unwrap_or(hwprobe_core::model::ALL_CATEGORIES));
        registry.stop_poller();
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        print_log();
        return;
    }

    if watch {
        let iv = interval
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);
        watch_loop(&registry, category, iv);
        registry.stop_poller();
        print_log();
        return;
    }

    // 一次性 dump
    registry.stop_poller();
    print_tree(&registry, category);
    print_log();
}

fn print_tree(registry: &Registry, category: Option<u32>) {
    for cat in Category::ALL {
        if let Some(c) = category {
            if c != cat as u32 {
                continue;
            }
        }
        let count = registry.device_count(cat as u32).unwrap_or(0);
        if !registry.category_present(cat) && count == 0 {
            println!("== {} == (不可用)", cat.name());
            continue;
        }
        println!("== {} ==", cat.name());
        for d in 0..count {
            let Ok(dev) = registry.device(cat as u32, d) else { continue };
            println!(
                "  [{}] {}  (vendor: {}, model: {}, serial: {})",
                d,
                dev.meta.name,
                empty_dash(&dev.meta.vendor),
                empty_dash(&dev.meta.model),
                empty_dash(&dev.meta.serial)
            );
            for (s, sen) in dev.sensors.iter().enumerate() {
                let kind = match sen.desc.kind {
                    SensorKind::Static => "静态",
                    SensorKind::Dynamic => "动态",
                };
                let status = status_text(sen.current.status);
                let value = format_value(&sen.current.value);
                let unit = &sen.desc.unit;
                let unit_sep = if unit.is_empty() { "" } else { " " };
                println!(
                    "    {:3}. {:40} [{:6}] = {}{}{} ({})",
                    s, sen.desc.name, kind, value, unit_sep, unit, status
                );
            }
        }
    }
}

fn watch_loop(registry: &Registry, category: Option<u32>, interval_ms: u64) {
    println!("watch 模式,Ctrl+C 退出。interval={}ms", interval_ms);
    // 首帧不出「等待首刷」:差分类要两个采样点
    registry.wait_ready(
        category.and_then(Category::from_u32),
        Duration::from_millis(2000),
    );
    loop {
        // 清屏简化:打印时间戳 + 动态值
        let cats: Vec<Category> = Category::ALL
            .iter()
            .copied()
            .filter(|c| category.map(|x| x == *c as u32).unwrap_or(true))
            .collect();
        let mut line = format!("\n[{}]", chrono_like_now());
        for cat in cats {
            let count = registry.device_count(cat as u32).unwrap_or(0);
            for d in 0..count {
                let Ok(dev) = registry.device(cat as u32, d) else { continue };
                for sen in dev.sensors.iter().filter(|s| s.desc.kind == SensorKind::Dynamic) {
                    if sen.current.status == hwprobe_core::model::Status::Ok {
                        line.push_str(&format!(
                            "  {}[{}]={}",
                            sen.desc.name,
                            dev.meta.name,
                            format_value(&sen.current.value)
                        ));
                    }
                }
            }
        }
        println!("{}", line);
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

/// stream 模式:订阅式推送验证 —— 注册一个回调,由 DLL 在内部线程上
/// 按节拍推送动态传感器快照,主线程只收队列,不再轮询。
fn cmd_stream(
    interval: &Option<String>,
    category: Option<u32>,
    count: &Option<String>,
    full: bool,
) {
    use hwprobe_core::stream::StreamBatch;
    use std::collections::HashMap;

    let registry = build_registry();
    // 等两个采样点到齐再出首批,避免首批全是"等待首刷"
    registry.wait_ready(
        category.and_then(Category::from_u32),
        Duration::from_millis(1500),
    );

    let interval_ms: u32 = interval
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let total: u64 = count
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX);
    let mask = match category {
        Some(c) => 1u32 << c,
        None => u32::MAX,
    };

    // (cat,dev,sen) -> (设备名, 传感器名, 单位) 展示用名称表
    let mut names: HashMap<(u32, u32, u32), (String, String, String)> = HashMap::new();
    for cat in Category::ALL {
        let n = registry.device_count(cat as u32).unwrap_or(0);
        for d in 0..n {
            let Ok(dev) = registry.device(cat as u32, d) else { continue };
            for (s, sen) in dev.sensors.iter().enumerate() {
                if sen.desc.kind == SensorKind::Dynamic {
                    names.insert(
                        (cat as u32, d, s as u32),
                        (
                            dev.meta.name.clone(),
                            sen.desc.name.clone(),
                            sen.desc.unit.clone(),
                        ),
                    );
                }
            }
        }
    }

    let (tx, rx) = std::sync::mpsc::channel::<StreamBatch>();
    let handle = registry.subscribe(
        interval_ms,
        mask,
        std::sync::Arc::new(move |b: &StreamBatch| {
            let _ = tx.send(b.clone()); // 回调内只做拷贝入队,立即返回
        }),
    );
    println!(
        "stream:已订阅(interval={interval_ms}ms,mask={mask:#010x},全库 {} 个动态传感器,按 mask 过滤后推送),收到 {total} 批后退出,Ctrl+C 随时退出",
        names.len()
    );

    let mut shown = 0u64;
    for b in rx.iter() {
        let t = b.timestamp_ms;
        let secs = t / 1000;
        println!(
            "\n#{} @ {:02}:{:02}:{:02}.{:03} —— {} 项",
            b.seq,
            (secs / 3600) % 24,
            (secs / 60) % 60,
            secs % 60,
            t % 1000,
            b.items.len()
        );
        for it in &b.items {
            let key = (it.category, it.device, it.sensor);
            let status = it.status;
            if !full && status != hwprobe_core::model::Status::Ok as u32 {
                continue;
            }
            let (dev, sen, unit) = names.get(&key).cloned().unwrap_or_default();
            let unit_sep = if unit.is_empty() { "" } else { " " };
            if full {
                println!(
                    "  {}[{}] / {}[{}] = {}{}{} (status={})",
                    it.category, it.device, it.sensor, sen,
                    format_value(&it.value), unit_sep, unit, status
                );
            } else {
                println!("  {} = {}{}{} ({})", sen, format_value(&it.value), unit_sep, unit, dev);
            }
        }
        shown += 1;
        if shown >= total {
            break;
        }
    }
    let _ = registry.unsubscribe(handle);
    registry.stop_poller();
    println!("\nstream:已退订,退出");
}

fn chrono_like_now() -> String {
    let ms = hwprobe_core::util::now_ms();
    let secs = ms / 1000;
    // 简单的 UTC 时分秒展示
    format!("{:02}:{:02}:{:02}.{:03}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60, ms % 1000)
}

fn empty_dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::F64(x) => {
            if x.fract() == 0.0 && x.abs() < 1e15 {
                format!("{}", *x as i64)
            } else {
                format!("{:.1}", x)
            }
        }
        Value::U64(x) => x.to_string(),
        Value::I64(x) => x.to_string(),
        Value::Bool(x) => x.to_string(),
        Value::Str(x) => x.clone(),
    }
}

fn status_text(s: hwprobe_core::model::Status) -> String {
    use hwprobe_core::model::Status::*;
    match s {
        Ok => "OK".into(),
        Waiting => "等待首刷".into(),
        NoDriver => "需要驱动".into(),
        NotSupported => "不支持".into(),
        NoPermission => "无权限".into(),
        Error => "错误".into(),
        other => format!("status={:?}", other),
    }
}
