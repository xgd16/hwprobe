# macOS AppleSMC 键位调研（Apple Silicon M1–M4 / Intel Mac）— 供 Rust 硬件监控库使用

调研方式：抓取 GitHub 一手源码/文档（exelban/Stats、vladkens/macmon、hholtmann/smcFanControl、beltex/SMCKit、acidanthera/VirtualSMC 文档、lavoiesl/osx-cpu-temp、Chris911/iStats）+ 相关 issue 讨论。凡未被两个以上独立来源证实的条目均标注「单来源」或「存疑」。

**仓库更正**（用户提问中存疑的仓库名）：
- macmon = **github.com/vladkens/macmon**（Rust，sudoless，Apple Silicon）— 不是 jitouch/dseptember/visusnet。
- 「smc-keys 数据库」没有找到 jedibacchus 仓库；最接近权威的键位文档是 **acidanthera/VirtualSMC 的 Docs/SMCSensorKeys.txt、SMCKeys.txt**（从 Apple libSMC.dylib/固件逆向得出）+ insanelymac「SMC keys knowledge database」帖。

---

## 0. 数据类型与换算公式（先看这个）

| 类型 | 字节 | 语义 | 换算公式 | 来源 |
|---|---|---|---|---|
| `sp78` | 2 (BE) | 有符号定点 7.8 | `value = i16_be / 256`（温度 °C、功率 W、电压 V） | Stats smc.swift、smcFanControl |
| `sp96` | 2 (BE) | 有符号定点 9.6 | `value = i16_be / 64` | Stats smc.swift |
| `spa5` | 2 (BE) | 有符号定点 10.5 | `value = u16_be / 32` | Stats smc.swift |
| `sp87` | 2 (BE) | 8.7 | `value = i16_be / 128` | Stats smc.swift |
| `flt ` | 4 (LE) | IEEE754 float32 | `f32::from_le_bytes`（Apple Silicon 传感器多为此） | Stats smc.swift、macmon sources.rs |
| `fpe2` | 2 (BE) | 无符号 14.2 定点 | `value = u16_be / 4 = (b0<<6) \| (b1>>2)`（风扇 RPM） | Stats、SMCKit、macmon 测试用例 `[0x13,0x88] → 1250` |
| `ui8 ` / `ui16` / `ui32` | 1/2/4 (BE) | 无符号整数 | 直接取值（FNum、电池 mA/mAh/mV） | Stats、macmon |
| `flag` | 1 | 布尔 | 0/1 | VirtualSMC SMCKeys.txt |
| `{fds` | 16 | 自定义 struct | 风扇名 = bytes[4..16] 的 ASCII | SMCKit、smcFanControl |

> **实现提示**：不要假设类型，先用 `readKeyInfo`（SMC selector 2, data8=9）拿 dataType/dataSize 再解码。macmon 的做法是按 dataType 分支（flt/fpe2/ui8/ui16/ui32）。Rust 里打开 `AppleSMC` 服务（macmon 专门找名字为 `AppleSMCKeysEndpoint` 的实例）→ `IOServiceOpen` → `IOConnectCallStructMethod(conn, 2, ...)`，data8=9 查 keyInfo、5 读字节、8 按索引枚举（`#KEY` 给出 key 总数）。

---

## 1. CPU 温度键

### 1.1 Intel Mac（多来源证实，放心用）

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 |
|---|---|---|---|---|---|
| `TC0P` | sp78 | CPU proximity（封装附近温度，最通用的「CPU 温度」） | °C = raw/256 | Intel 全系 | [VirtualSMC SMCKeys.txt](https://github.com/acidanthera/VirtualSMC/blob/master/Docs/SMCKeys.txt)、[osx-cpu-temp smc.h](https://github.com/lavoiesl/osx-cpu-temp/blob/master/smc.h)、[iStats smc.h](https://github.com/Chris911/iStats/blob/master/ext/osx_stats/smc.h)、[Stats values.swift](https://github.com/exelban/stats/blob/master/Modules/Sensors/values.swift) |
| `TC0D` | sp78 | CPU die 二极管（每物理 CPU 一个） | raw/256 | Intel（legacy，Macmini6,2 等） | VirtualSMC、Stats |
| `TC0E` | sp78 | PECI die **滤波（虚拟）**温度 | raw/256 | Intel（iMac16,1 起） | VirtualSMC、Stats |
| `TC0F` | sp78 | PECI die 滤波+校正（供风扇/功耗控制，TC0F=TC0E+TC0G） | raw/256 | Intel | VirtualSMC、Stats |
| `TC0H` | sp78 | CPU heatsink | raw/256 | Intel（legacy，iMac12,2） | VirtualSMC、Stats |
| `TC0c`…`TC9c` | sp78 | 每物理核心温度（PECI） | raw/256 | Intel（iMacPro1,1 等，最多见到 10 核） | VirtualSMC |
| `TC%c`/`TC%C` | sp78 | 核心 % 温度（Stats 通配写法） | raw/256 | Intel | Stats |
| `TC0T` | sp78 | die 温度趋势（trend） | raw/256 | Intel（iMac16,1+） | VirtualSMC |
| `TCAD`/`TCAC`/`TCBC`/`TCAH`/`TCBH` | sp78 | MacPro5,1 双路 CPU 变体（package/heatsink/相对 ProcHot），疑已废弃 | raw/256 | MacPro5,1 legacy | VirtualSMC（标注 deprecated）、Stats |

### 1.2 Apple Silicon（社区推断，**键→核的映射随芯片代际/机型变化，慎用**）

类型：**`flt `（4 字节小端 float，°C）** —— 由 macmon 证实（它只保留 `read_float_val` 成功即 type=="flt " 的键）：[macmon metrics.rs](https://github.com/vladkens/macmon/blob/main/src/metrics.rs)。macmon 同时注明：**SMC 上这些键在 macOS 14+ 可读；macOS 13 上要用 HID**。

| key | 类型 | 含义（Stats 命名） | 换算 | 适用机型 | 来源 / 可信度 |
|---|---|---|---|---|---|
| `Tp09`,`Tp0T` | flt | CPU 能效核 E-core 1–2 | float | M1/M1 Pro/Max/Ultra | [Stats values.swift](https://github.com/exelban/stats/blob/master/Modules/Sensors/values.swift)（单来源但被大量复用） |
| `Tp01`,`Tp05`,`Tp0D`,`Tp0H`,`Tp0L`,`Tp0P`,`Tp0X`,`Tp0b` | flt | CPU 性能核 P-core 1–8 | float | M1 系 | Stats；[#1012](https://github.com/exelban/stats/issues/1012) 交叉提到 `Tp05`=M1 P-core 2 |
| `Tp1h`,`Tp1t`,`Tp1p`,`Tp1l` | flt | E-core 1–4 | float | M2 系 | Stats（**与 issue #1012 的 M2 结论冲突，见下**） |
| `Tp01`,`Tp05`,`Tp09`,`Tp0D`,`Tp0X`,`Tp0b`,`Tp0f`,`Tp0j` | flt | P-core 1–8 | float | M2 系（含 Pro/Max 的 8P 版本） | Stats master |
| `Te05`,`Te0L`,`Te0P`,`Te0S` | flt | E-core 1–4 | float | M3 系 | Stats、[#1703](https://github.com/exelban/stats/issues/1703)（M3 键位来源） |
| `Tf04`,`Tf09`,`Tf0A`,`Tf0B`,`Tf0D`,`Tf0E`,`Tf44`,`Tf49`,`Tf4A`,`Tf4B`,`Tf4D`,`Tf4E` | flt | P-core 1–12 | float | M3 系 | Stats、#1703 |
| `Te05`,`Te0S`,`Te09`,`Te0H` | flt | E-core 1–4 | float | M4 系 | Stats |
| `Tp01`,`Tp05`,`Tp09`,`Tp0D`,`Tp0V`,`Tp0Y`,`Tp0b`,`Tp0e` | flt | P-core 1–8 | float | M4 系 | Stats |
| `Ts*` | flt | skin/外壳类（macmon 把 Ts 也归入 CPU 组求平均） | float | AS | macmon metrics.rs（单来源，语义存疑） |

**⚠️ 重大存疑（必须写进代码注释）**：
1. 同名键跨代语义不同：Stats issue #1012 明确 `Tp05` 在 M1 = P-core 2，而 **M2 = E-core 1**（该 issue 的 M2 表：`Tp05/Tp0D/Tp0j/Tp0r`=E-core，`Tp01/Tp09/Tp0f/Tp0n`=P-core）；但 Stats 当前 master 的 M2 表又改成 `Tp1h/…` 为 E-core、`Tp05` 为 P-core 2。两份记录互相矛盾，可能因 M2 / M2 Pro/Max 机型不同，也可能社区判断有误。**结论：AS 的 per-core 映射只可作参考，不能当作契约。**
2. 用户问的 **`Tp0J`（大写 J）在任何来源中都不存在** —— 疑为 `Tp0j`（小写，M2）之误；大写 J 不要实现。
3. `Te05` = M3/M4 的 E-core 1（Stats），不是温度传感器编号泛指。

**更稳的 AS 做法（macmon 采用）**：枚举所有 `Tp`/`Te`/`Tf`/`Tg`/`Ts` 前缀的键按 CPU/GPU 分组求平均；或走 IOHID（AppleVendor 页 0xff00 / usage 0x0005），读 `pACC MTR Temp Sensor*`（P 核）、`eACC MTR Temp Sensor*`（E 核）、`GPU MTR Temp Sensor*`、`SOC/ANE/ISP/PMGR/PMU …`（见 [Stats values.swift 的 HIDSensorsList](https://github.com/exelban/stats/blob/master/Modules/Sensors/values.swift)、[macmon sources.rs](https://github.com/vladkens/macmon/blob/main/src/sources.rs)，后者引用 freedomtan/sensors）。

---

## 2. GPU 温度键

### 2.1 Apple Silicon（`flt`，含义=「GPU 传感器 n」，无官方名称）

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 |
|---|---|---|---|---|---|
| `Tg05`,`Tg0D`,`Tg0L`,`Tg0T` | flt | GPU 传感器 1–4（按编号平均即「GPU 温度」） | float | M1 系 | Stats values.swift（单来源） |
| `Tg0f`,`Tg0j` | flt | GPU 1–2 | float | M2 系 | Stats master（issue #1012 早期为 `Tg0f`,`Tg0n` → 存疑） |
| `Tf14`,`Tf18`,`Tf19`,`Tf1A`,`Tf24`,`Tf28`,`Tf29`,`Tf2A` | flt | GPU 1–8 | float | M3 系 | Stats、#1703 |
| `Tg0G`,`Tg0H`（M4）；`Tg1U`,`Tg1k`（M4 Pro/Max）；`Tg0K`,`Tg0L`,`Tg0d`,`Tg0e`,`Tg0j`,`Tg0k` | flt | GPU 1–8 | float | M4 系 | Stats（单来源） |

### 2.2 Intel / AMD 独显（sp78，多来源）

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 |
|---|---|---|---|---|---|
| `TG0P` | sp78 | GPU proximity | raw/256 | Intel（osx-cpu-temp 的 GPU 温度就用它） | [osx-cpu-temp](https://github.com/lavoiesl/osx-cpu-temp/blob/master/smc.h)、VirtualSMC、Stats |
| `TG0D` | sp78 | GPU diode | raw/256 | Intel | VirtualSMC、Stats |
| `TG0H` | sp78 | GPU heatsink | raw/256 | Intel | Stats |
| `TGDD` | sp78 | GPU（AMD Radeon 独显） | raw/256 | Intel+AMD | Stats（单来源） |
| `TG0E`/`TG0F` | sp78 | GPU die 滤波温度 / 滤波+校正（风扇控制用） | raw/256 | Intel | VirtualSMC |
| `TCGC` | sp78 | Intel 核显（CPU Gfx Core） | raw/256 | Intel | VirtualSMC、Stats |

---

## 3. 功耗键

### 3.1 Apple Silicon（**SMC 上可依赖的只有 `PSTR`**）

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 / 可信度 |
|---|---|---|---|---|---|
| `PSTR` | **flt** | 整机总功耗（System Total Power，延迟 1 s） | W = float | AS（实测 M1–M5） | **多来源**：macmon `get_sys_power()` 直接 `read_float_val("PSTR")`（[metrics.rs](https://github.com/vladkens/macmon/blob/main/src/metrics.rs)）、VirtualSMC（Intel 上 sp78，同一语义）、Stats |
| `PDTR` | flt?/sp 系 | DC-In/适配器总输入功率 | 视类型 | AS 上未证实 | Stats 列为 all-platform（单来源尝试），存疑 |
| `PMTR` | spa5/sp? | 内存总功耗 | 视类型 | AS 上未证实（macmon 的 RAM 功耗走 IOReport `DRAM*` 能量计数，不是 SMC） | VirtualSMC（Intel）、Stats；AS 存疑 |
| `PCPC`/`PCPG`/`PCPT`/`PCTR` | sp 系 | CPU package / GPU 功耗 | 视类型 | **Intel 语义**（见 3.2）；AS 上未获证实 | VirtualSMC；AS 存疑 |
| `PDBR` | ? | 屏幕背光功耗 | ? | AS（Stats 标 m1Gen+m4Gen） | 仅 Stats，单来源 |

> **AS CPU/GPU/ANE/RAM 功耗的正确姿势**：IOReport（`Energy Model` 组的 `CPU Energy`/`GPU Energy`/`ANE*`/`DRAM*`/`GPU SRAM*` 通道，单位 mJ/uJ/nJ，W = 能量/采样时长）或 AppleCLPC 驱动的隐藏 energy 计数 —— 见 [macmon readme](https://github.com/vladkens/macmon/blob/main/readme.md) 与 [docs/clpc-discovery.md](https://github.com/vladkens/macmon/blob/main/docs/clpc-discovery.md)。不要臆造 AS 的 SMC CPU/GPU 功耗键。

### 3.2 Intel Mac（VirtualSMC SMCKeys.txt 给出权威类型；Stats 复用）

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 |
|---|---|---|---|---|---|
| `PC0C` | sp78（MacPro6,1 为 spa5） | CPU 核心功耗（=IC0C×VC0C） | W = raw/256 或 raw/32 | Intel | VirtualSMC、Stats |
| `PC0G` | sp96 | CPU 内置 GPU（AXG）功耗 | raw/64 | Intel（iMac17,1） | VirtualSMC、Stats |
| `PC0R` | sp78 | CPU high-side 总功率（IC0R×VD0R） | raw/256 | Intel | VirtualSMC、Stats |
| `PC0I`/`PC0M`/`PC0S` | sp96 | CPU I/O / 内存 / System Agent | raw/64 | Intel（iMac17,1） | VirtualSMC |
| `PCPC` | sp78（多种类型） | CPU package core power（PECI） | 按 dataType | Intel（MacBookPro12,1 等） | VirtualSMC、Stats |
| `PCPG` | sp78（多种） | CPU package GFX power（PECI） | 按 dataType | Intel | VirtualSMC、Stats |
| `PCPT` | sp78/spa5 | CPU package **总**功率（PECI） | 按 dataType | Intel（MacPro6,1） | VirtualSMC、Stats |
| `PCTR` | sp96/spa5 | CPU 总功率（PC0C+PC0G，或含 PC0I/PC0M/PC0S） | raw/64 或 /32 | Intel | VirtualSMC、Stats |
| `PCPR` | sp96 | CPU package total power（SMC 侧） | raw/64 | Intel | VirtualSMC、Stats |
| `PC2C`/`PC3C`/`PCAC`/`PCAM`/`PCSC`/`PCEC`/`PCGC`/`PCGM` | **flt**（4B） | VCCSA / DDR / core / core-IMON / EDRAM / 核显 IMON 等子项 | float（W） | Intel（MacBookPro11,1–13,2、iMac18,x） | VirtualSMC、Stats |
| `PDTR` | sp78/sp96 | DC-In 总输入功率（ID0R×VD0R） | raw/256 或 /64 | Intel（iMac18,3） | VirtualSMC、Stats |
| `PSTR` | sp78（也见 sp87/sp96/spa5） | 系统总功耗（延迟 1 s） | 按 dataType | Intel | VirtualSMC、Stats |
| `PMTR` | spa5 | 内存总功率 | raw/32 | Intel | VirtualSMC、Stats |
| `PPBR` | sp78/sp87/sp96 | 电池供电时的 PBus 功率（BMON） | 按 dataType | Intel | VirtualSMC、Stats |
| `PB0R` | sp78/sp87 | 电池放电功率（IB0R×VP0R） | 按 dataType | Intel | VirtualSMC |
| `PCAB`/`PCAC`/`PCBB`/`PCBC` | ? | MacPro 双路 A/B 核功率 | 按 dataType | Intel（MacPro） | 仅 Stats，单来源 |

---

## 4. 风扇键（多来源，结论一致）

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 |
|---|---|---|---|---|---|
| `FNum` | `ui8 ` | 风扇数量 | 直接取值 | 所有带风扇的 Mac | smcFanControl smc.c、SMCKit、iStats |
| `F0Ac`/`F1Ac` | **`fpe2`**（AS 上偶见 `flt`，macmon 两者都支持） | 当前转速 | **RPM = u16_be / 4**（等价 `(b0<<6)\|(b1>>2)`；macmon 测试 `0x1388 → 1250 RPM`） | 全部（fanless 机型无） | [VirtualSMC SMCSensorKeys.txt](https://github.com/acidanthera/VirtualSMC/blob/master/Docs/SMCSensorKeys.txt)（F0Ac type fpef/fpe2）、SMCKit、smcFanControl、iStats、macmon |
| `F0Mn`/`F0Mx` | fpe2 | 最小/最大转速 | u16_be/4 | 全部 | SMCKit、smcFanControl、iStats、macmon（F*Ac 的后缀换 Mx 即最大转速） |
| `F0Sf` | fpe2 | 安全转速 | u16_be/4 | Intel | smcFanControl（单来源） |
| `F0Tg` | fpe2 或 `flt`（AS 上见过 flt） | 目标转速（可写，需特权） | 同对应类型 | 全部 | Stats smc.swift（写入时按 dataType 分 flt/fpe2 分支） |
| `F0ID` | `{fds`（16B） | 风扇名 | bytes[4..16] ASCII，去尾空格 | 全部 | SMCKit、smcFanControl、Stats |
| `F0Md`/`F0md` | ui8 | 模式：0=auto，1=forced（AS 用小写 `md`，Stats 先探测） | 直接 | 全部 | Stats smc.swift、smcFanControl |
| `FS! ` | ui16 | 各风扇强制模式位掩码（bit i = fan i forced） | 直接 | Intel | smcFanControl、Stats |
| `Ftst` | ui8 | AS 风扇解锁（写 1 让 thermalmonitord 让出控制，M1–M4；M5+ 无此键） | 直接 | AS | Stats smc.swift（单来源） |

**回答「F0Ac 是 fpe2 吗？RPM = raw 还是 raw/4」**：是 `fpe2`（14 位整数 + 2 位小数的大端定点），所以 **RPM = 大端 u16 / 4**；Stats 的 `(b0<<6)+(b1>>2)` 是同一算式取整。**不要**再除以 4 之外的系数，也**不要**假设一定是 fpe2 —— 个别平台给 `flt`，按 dataType 分支最稳。

---

## 5. 主板 / 内存 / SSD / 电池温度

| key | 类型 | 含义 | 换算 | 适用机型 | 来源 / 可信度 |
|---|---|---|---|---|---|
| `TB0T` | sp78 | 电池温度 | raw/256 | Intel（iStats 的电池温度键） | VirtualSMC、[iStats](https://github.com/Chris911/iStats/blob/master/ext/osx_stats/smc.h) |
| `TB1T`/`TB2T` | sp78 | 电池热敏 0/1（Stats 在 AS 上标为 Battery 1/2） | raw/256 | Intel + AS | VirtualSMC、Stats |
| `TH0x` | sp78 | SSD/NAND（Drive 0）温度 | raw/256 | AS（Stats「NAND」）+ Intel SSD | VirtualSMC、Stats |
| `TH0A`/`TH0B`/`TH0C`…`TH%x` | sp78 | 硬盘/SSD 仓位 A/B/C | raw/256 | Intel | Stats（单来源命名） |
| `TM0P`（大写 M/P） | sp78 | DIMM/内存 proximity（也有条目写「Memory proximity filtered」） | raw/256 | Intel | VirtualSMC |
| `TM0D`…`TM3D`/`TM0p`…`TM3p` | sp78 | 各 DIMM PECI/proximity | raw/256 | Intel（MacPro/iMac 多内存槽） | VirtualSMC |
| `Tm02`,`Tm06`,`Tm08`,`Tm09` | flt | Memory 传感器 1–4 | float | AS M1 系 | Stats（单来源） |
| `Tm0p`,`Tm1p`,`Tm2p` | flt | Memory Proximity 1–3 | float | AS M4 系 | Stats（单来源） |
| `Tm0P`（T 大写 m 小写） | sp78 | **语义混乱**：VirtualSMC 同一 key 有「MLB top side temp」「Battery Charger Prox」「EMC1428 内部二极管」等多种条目 | raw/256 | Intel（依机型） | VirtualSMC → **存疑，别单独当内存温度用** |
| `Tm0P` vs `Tm0p` | | **大小写是不同 key**（cooked vs raw/不同传感器） | | | VirtualSMC |
| `Tp0P`（Intel） | sp78 | 电源板 / MLB / 电源 proximity（VirtualSMC 多条目） | raw/256 | Intel | VirtualSMC、Stats |
| `Tp0T`（Intel） | sp78 | PSU 次级散热片二极管 | raw/256 | Intel | VirtualSMC |
| `TW0P` | sp78 | Airport/WiFi 卡温度 | raw/256 | Intel + AS | VirtualSMC、Stats |
| `TL0P` | sp78 | LCD/显示屏 | raw/256 | Intel + AS | VirtualSMC、Stats |
| `TA0V` | sp78 | 环境温度（ambient） | raw/256 | Intel | osx-cpu-temp |
| `TA%P` | sp78 | Ambient % | raw/256 | Intel + AS | Stats |
| `Ts0P` | sp78 | 掌托/皮肤温度 | raw/256 | Intel | VirtualSMC、Stats |
| `TI%P`/`TTLD`/`TR?D` | sp78 | Thunderbolt | raw/256 | Intel + AS | Stats |
| `Tp01`/`Tp05`（AS） | flt | **与 Intel 的 `Tp0P`/`Tp0T` 同前缀但完全无关**（AS 是 CPU 核） | float | AS | Stats —— 实现时必须按平台分表！ |

AS 上还有 HID 温度（**非 SMC**，但同一个监控库值得支持）：`pACC MTR Temp Sensor*`、`eACC MTR Temp Sensor*`、`GPU MTR Temp Sensor*`、`SOC/ANE/ISP MTR Temp Sensor*`、`PMGR SOC Die Temp Sensor*`、`PMU tdev*/tdie*`、`gas gauge battery`、`NAND CH% temp`（Stats HIDSensorsList；macmon sources.rs 用同一 IOHID 路径）。

---

## 6. 读取权限与 macOS 版本差异

| 事项 | 结论 | 来源 |
|---|---|---|
| 读 SMC 键（温度/风扇/功率/电池） | **普通用户可读，无需 root/entitlement**。打开 `AppleSMC`（macmon 特指 `AppleSMCKeysEndpoint`）user client 即可 | macmon README「Runs without sudo」+ sources.rs；Stats 读传感器也无需 helper |
| 读 IOHID 温度（AS） | 普通用户可读 | macmon、Stats |
| 读 IOReport 能量计数 | 传统上普通用户可读；**macOS 27 起 `Energy Model` 不再刷新，需 `com.apple.private.pmgr.nrg.reporting` entitlement**（root 也拿不到），macmon 转用 AppleCLPC 隐藏计数绕过 | [macmon readme](https://github.com/vladkens/macmon/blob/main/readme.md) |
| 写风扇（F0Tg / F0Md / FS! / Ftst） | **需要 root 或特权 helper**：smcFanControl 用 sudo；Stats 安装 SMJobBless helper `eu.exelban.Stats.SMC.Helper` | smcFanControl、Stats（helper 亦见 [CVE-2025-21606](https://nvd.nist.gov/vuln/detail/CVE-2025-21606)） |
| `powermetrics`（对照） | 需要 root（这正是 macmon 存在的原因） | macmon README |
| 个别 key 读不出来 | 与权限无关：SMC 固件层 `ATTR_PRIVATE_READ`/`KPST` 保护返回 `SmcNotReadable(0x85)`；key 不存在返回 `0x84` | [VirtualSMC SMCKeys.txt](https://github.com/acidanthera/VirtualSMC/blob/master/Docs/SMCKeys.txt)、Stats smc.swift（把 0x84 当 not found） |
| macOS 13（Ventura） | AS 上 per-core SMC 温度键（Tp/Te/Tg）可能不可读/不存在，macmon 走 HID fallback；**macOS 14+ 才有这些 SMC 键** | macmon metrics.rs 注释：「HID for M1, SMC for M2/M3 … SMC available from macOS 14」 |
| macOS 14/15（Sonoma/Sequoia） | AS SMC 温度/风扇/PSTR 可用；未见读取收紧 | macmon、Stats 实践 |
| macOS 26（Tahoe） | 读取无变化；但 **SMJobBless 风扇 helper 安装被 `smd` 拒绝**（签名/`SMAuthorizedClients` 校验收紧）→ 风扇**写入**受影响 | [Stats issue #3193](https://github.com/exelban/stats/issues/3193) |
| macOS 27 | 见上：IOReport Energy Model 根less 失效 | macmon README |

---

## 7. 可信度分级速查

**多来源证实（≥2 独立项目/文档一致）**：`TC0P/TC0D/TC0E/TC0F/TC0H`、`TG0P/TG0D`、`PC0C/PC0R/PCPC/PCPG/PCPT/PCTR/PDTR/PSTR/PMTR/PC3C`、`FNum/F0Ac/F0Mn/F0Mx/F0ID`、`TB0T`、`TW0P`、`TL0P`、`TH0x`、fpe2/sp78/flt 换算规则、无 root 可读 / 写风扇需特权。

**单来源但可信（Stats/macmon 实测代码）**：AS 各代 Tp/Te/Tf/Tg 键表（Stats）、`PSTR`=flt（macmon）、AS `Tm02/06/08/09`、`Tm0p/1p/2p`、M4 的 Tg 组、`PDBR`、`Ftst`、`F0Sf`。

**存疑（勿写死）**：
- M2 的 Tp 键映射（Stats master 与 issue #1012 结论相反）。
- `Tp0J`（大写 J）——不存在，疑为 `Tp0j` 误写。
- AS 上 `PDTR/PMTR/PCPC/PCPG` 是否真实存在 —— 无实测证据；AS 功耗请走 IOReport/CLPC。
- `Tm0P`（Intel）在不同机型是内存/充电器/MLB 温度，语义不唯一。
- `TGDD`、`PCAB/PCAC/PCBB/PCBC`、`PBwo`、`Ts*` 归属。

---

## 8. Rust 监控库「最小可放心键集合」建议

**通用（两平台都先做）**
1. 运行时枚举：读 `#KEY`（ui32，key 总数）→ data8=8 按索引列 key → data8=9 查 dataType/dataSize → data8=5 读值；按 `sp78/sp96/spa5/flt/fpe2/ui8/ui16/ui32` 分支解码，缓存 keyInfo。**键的存在性、类型都以运行时探测为准，硬编码表只做「已知别名/语义标注」。**

**Intel 表**
- CPU 温度：`TC0P`（fallback `TC0E` → `TC0F` → `TC0D` → `TC0H`）
- GPU 温度：`TG0P`（fallback `TG0D`，AMD 独显 `TGDD`）
- 功耗：CPU `PC0C`（fallback `PCTR`/`PCPT`/`PCPC`）、GPU `PC0G`、整机 `PSTR`、输入 `PDTR`、内存 `PMTR`/`PC3C`
- 风扇：`FNum` + 每扇 `F{n}Ac/F{n}Mn/F{n}Mx/F{n}ID`（+ 可选 `F{n}Tg` 写）
- 板级：`TB0T`、`TM0P`、`TH0x`/`TH%x`、`TW0P`、`TL0P`、`TA0V`

**Apple Silicon 表**
- CPU/GPU 温度：**枚举** `Tp*/Te*/Tf*`（CPU）与 `Tg*`（GPU）求平均；或 IOHID `pACC/eACC/GPU MTR Temp Sensor*`。若必须硬编码，按代际表（M1/M2/M3/M4 各一套，见 §1.2/§2.1）并把「映射可能因机型而异」写进文档与日志。
- 功耗：SMC 只用 `PSTR`（flt，整机 W）；CPU/GPU/ANE/RAM 功耗走 IOReport `Energy Model`（W = 能量(mJ/uJ/nJ)/Δt），并为 macOS 27+ 预留 CLPC 路径。
- 风扇：同 Intel（`FNum/F{n}Ac/Mn/Mx/ID`；`F{n}Ac` 可能是 fpe2 或 flt）；MacBook Air 无风扇 → `FNum`=0 或键缺失。
- 板级：`TB1T/TB2T`、`TH0x`（NAND）、内存 `Tm02/Tm06/Tm08/Tm09`（M1）或 `Tm0p/Tm1p/Tm2p`（M4）、`TW0P`。
- 温度键类型按 `flt` 优先、其它类型兜底。

**权限 API 设计**：读路径不请求任何特权；风扇写路径单独 feature（需要 root/SMAppService/SMJobBless helper，并注意 macOS 26 的 helper 安装限制）；所有 key 读取统一 `Option<f32>` + 明确的 `KeyNotFound(0x84)`/`NotReadable(0x85)` 错误分支。

---

### 附：本次核对过的来源清单
- https://github.com/exelban/stats/blob/master/Modules/Sensors/values.swift （AS/Intel 键位与命名，含 M5/M6/A18，最新 master）
- https://github.com/exelban/stats/blob/master/SMC/smc.swift （类型解码、#KEY 枚举、0x84/0x85 语义、AS 风扇 Ftst/F0md）
- https://github.com/acidanthera/VirtualSMC/blob/master/Docs/SMCSensorKeys.txt （传感器键类型/含义，源自 Apple libSMC）
- https://github.com/acidanthera/VirtualSMC/blob/master/Docs/SMCKeys.txt （公共/隐藏键、attr、功率键类型与机型）
- https://github.com/hholtmann/smcFanControl/blob/master/smc-command/smc.c （FNum/F%dID/Ac/Mn/Mx/Sf/Tg、FS!）
- https://github.com/beltex/SMCKit/blob/master/SMCKit/SMC.swift （FNum ui8、F%dID {fds、F%dAc/Mn/Mx fpe2）
- https://github.com/vladkens/macmon （readme.md、src/metrics.rs、src/sources.rs：AS 的 flt 温度、PSTR=flt、fpe2 换算、IOReport/CLPC、macOS 13/27 变化）
- https://github.com/lavoiesl/osx-cpu-temp/blob/master/smc.h （TC0P/TG0P/TA0V/F0Ac）
- https://github.com/Chris911/iStats （TC0P/F%dAc/F%dMn/F%dMx/FNum/TB0T）
- https://github.com/exelban/stats/issues/1012 （M2 键位与跨代冲突）
- https://github.com/exelban/stats/issues/1703 （M3 键位）
- https://github.com/exelban/stats/issues/3193 （macOS 26 风扇 helper 安装被拒）
