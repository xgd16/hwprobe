//! 一次性探针(macOS):枚举 AppleSMC 全部按键并打印类型与解码值。
//!
//! 用途:在具体机型上核对键位表(docs/smc-keys-research.md)哪些键真实存在、
//! 类型是什么、读出来的值是否合理。不是库的一部分。
//!
//! 运行:cargo run -p hwprobe-core --example dbg_mac_smc [-- all]
//!   默认只打印温度/风扇/功耗/电池相关键(前缀 T/F/P/B/#);
//!   `-- all` 打印全部键。

#[cfg(target_os = "macos")]
fn main() {
    use hwprobe_core::platform::macos::smc::Smc;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_all = args.iter().any(|a| a == "--all" || a == "all");

    let smc = match Smc::open() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SMC 打开失败: {e}");
            std::process::exit(1);
        }
    };

    // 快速模式:--keys k1 k2 ... 只读指定键并打印一行(用于快照对比实验)
    if let Some(pos) = args.iter().position(|a| a == "--keys") {
        for name in &args[pos + 1..] {
            let mut key = [0u8; 4];
            let b = name.as_bytes();
            if b.len() != 4 {
                eprintln!("键名必须 4 字符: {name}");
                continue;
            }
            key.copy_from_slice(b);
            match smc.read(&key) {
                Some(v) => println!("{name} {v:?}"),
                None => println!("{name} <读不到>"),
            }
        }
        return;
    }

    match smc.key_count() {
        Some(n) => println!("#KEY = {n}"),
        None => println!("#KEY 读不到(将按索引扫描到 4096)"),
    }
    println!("{:<6} {:<6} {}", "key", "type", "value");

    let max = smc.key_count().unwrap_or(4096);
    for i in 0..max {
        let Some(key) = smc.key_at_index(i) else {
            if i > 16 && smc.key_at_index(i + 1).is_none() && smc.key_at_index(i + 2).is_none() {
                break;
            }
            continue;
        };
        let name = String::from_utf8_lossy(&key).into_owned();
        let interesting = show_all
            || matches!(name.as_bytes()[0], b'T' | b'F' | b'P' | b'B' | b'#')
            || name.starts_with("TH")
            || name.starts_with("Tm");
        let Some((ty, bytes)) = smc.read_raw(&key) else {
            if interesting {
                println!("{name:<6} (读失败)");
            }
            continue;
        };
        let tyname = String::from_utf8_lossy(&ty.to_be_bytes()).into_owned();
        let val = hwprobe_core::platform::macos::smc::decode(ty, &bytes)
            .map(|v| format!("{v:?}"))
            .unwrap_or_else(|| format!("{bytes:02x?}"));
        if interesting {
            println!("{name:<6} {tyname:<6} {val}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("本探针仅支持 macOS");
}
