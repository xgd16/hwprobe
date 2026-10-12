# macOS 用户态硬件数据 IOKit/系统接口调研（Rust FFI 参考）

> 调研方法：① GitHub raw 源码（exelban/stats、smartmontools、vladkens/macmon、sebhildebrandt/systeminformation）；
> ② Apple SDK 头文件（phracker/MacOSX-SDKs 的 MacOSX10.11.sdk + 本机 Xcode macOS SDK）；
> ③ **本机实测**（Mac14,9 / MacBook Pro 14" M2 Pro / macOS 27.2 build 26B5101f / 普通用户 `xuguodong`，arm64）。
> 标注 ⚠️ 的条目为未能验证的存疑项，不要直接当作事实使用。

---

## 1. GPU 利用率 / 显存（IOAccelerator / PerformanceStatistics）

**匹配方式**：`IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching("IOAccelerator"), &it)`。
`kIOAcceleratorClassName = "IOAccelerator"`（IOAccelerator.h）。Apple Silicon 上实际类名为
`AGXAcceleratorG13X`（M1 Max，t600x）、`AGXAcceleratorG14X`（M2 Pro，t6020）等，均 `IOConformsTo("IOAccelerator")`；
Intel Mac 为 `IOGFXAccelerator`、`AMDRadeonX*`、`IntelAccelerator`、`NVDA`/`NVAccelerator`。

**读取方式**：accelerator 节点的 `PerformanceStatistics` 属性（CFDictionary，键为 CFString、值为 CFNumber/OSNumber）：
`IORegistryEntryCreateCFProperty(entry, CFSTR("PerformanceStatistics"), ...)` 或 `IORegistryEntryCreateCFProperties`。
**无需 root**：本机普通用户实测可读；exelban/stats 也以普通用户运行。

### 1.1 Apple Silicon AGX 实测 key 清单（AGXAcceleratorG14X，M2 Pro，macOS 27.2）

| key 字符串 | 实测值/类型 | 语义与单位 | 来源 |
|---|---|---|---|
| `"Device Utilization %"` | 55（整数） | GPU 总利用率，0–100（%）；Stats 中 >100 会被 clamp | 本机 ioreg 实测；[stats reader.swift](https://raw.githubusercontent.com/exelban/stats/master/Modules/GPU/reader.swift) |
| `"Renderer Utilization %"` | 55（整数） | 渲染器利用率 0–100（%） | 同上 |
| `"Tiler Utilization %"` | 55（整数） | Tiler 利用率 0–100（%） | 同上 |
| `"In use system memory"` | 1183580160（整数） | GPU 已用统一内存，**字节**；会超过 2^31，按 64 位读 | 本机实测；[M1 Max ioreg dump](https://gist.github.com/philipturner/48c72e3fcce0ce9489071eb083a5086e)（458113024） |
| `"In use system memory (driver)"` | 0（整数） | 驱动侧占用，字节 | 本机实测 |
| `"Alloc system memory"` | 3518152704（整数） | GPU 已分配统一内存总量，**字节**（>2^31，必须 64 位读） | 本机实测；gist（3730522112） |
| `"Allocated PB Size"` | 90570752（整数） | Page-buffer 分配大小，字节 | 本机实测、gist |
| `"TiledSceneBytes"` | 1474560 | 与分块渲染相关的字节计数（语义未文档化） | 本机实测、gist |
| `"SplitSceneCount"` / `"recoveryCount"` / `"lastRecoveryTime"` | 0 | 场景拆分/ GPU 重启计数与时间戳 | 本机实测、gist |
| `"model"` | `"Apple M2 Pro"` | GPU 型号字符串（accelerator 节点顶层属性，非字典内） | 本机实测 |
| `"gpu-core-count"` | 16 | GPU 核心数（节点顶层属性） | 本机实测 |
| `"AGCInfo"` | dict（`fLastSubmissionPID`,`fSubmissionsSinceLastCheck`,`fBusyCount`） | Stats 读 `poweredOffByAGC` 判断 GPU 电源状态（本机字典中未见该键） | 本机实测、[stats](https://raw.githubusercontent.com/exelban/stats/master/Modules/GPU/reader.swift) |

### 1.2 Stats 支持的跨厂商 key（源码字符串，逐字核对）

| key 字符串 | 类型 | 语义 | 来源 |
|---|---|---|---|
| `"Device Utilization %"` ?? `"GPU Activity(%)"` | Int | 利用率 0–100；Stats 两个 key 依次 fallback | [exelban/stats reader.swift L139](https://raw.githubusercontent.com/exelban/stats/master/Modules/GPU/reader.swift) |
| `"Renderer Utilization %"` / `"Tiler Utilization %"` | Int | 0–100 | 同上 L140-141 |
| `"Temperature(C)"` | Int | **摄氏度**（注意带 `(C)`） | 同上 L142 |
| `"Fan Speed(%)"` | Int | 风扇 0–100%（注意 `Speed` 大写 S） | 同上 L143 |
| `"Core Clock(MHz)"` / `"Memory Clock(MHz)"` | Int | 核心/显存频率 MHz | 同上 L144-145 |

### 1.3 ⚠️ 存疑 key（未能找到一手验证）

- `"Texture memory in use"`：常见于 Intel `IntelAccelerator` 的 PerformanceStatistics 传闻，未验证。
- `"VRAM,totalMB"`：**不在 PerformanceStatistics**；实测/来源显示它在 PCI 显示设备（`IODeviceTree:/efi/platform` 的 display
  IOPCIDevice / Hackintosh 的 GFX0）属性上，同级还有 `vramFreeBytes`（[qwq.ren 博客](https://blog.qwq.ren/posts/frozen-screen-macos-vm-memory-starvation/)：`VRAM,totalMB=16384`、`vramFreeBytes=15728640`）。
- `"Fan speed(%)"`（小写 s）、`"Temperature"`（无单位后缀）、`"Total Power(w)"`、`"Power(?)"`：未验证。GPU 功率在
  Apple Silicon 并非来自 IORegistry，而是 **IOReport 私有 API**：group `CLPC` / subgroup `Energy Counters` /
  channel `GPU Energy`（单位 nJ），另有 group `Energy Model` 的 `GPU Energy`、`GPU SRAM*`（mJ）
  —— [vladkens/macmon shared.rs](https://raw.githubusercontent.com/vladkens/macmon/main/src/shared.rs)（powermetrics 同源，免 root）。

---

## 2. 磁盘统计（IOBlockStorageDriver / IOBlockStorageDevice / IOMedia）

### 2.1 `IOBlockStorageDriver` 节点 `"Statistics"` 字典（全部 OSNumber）

来源：[IOBlockStorageDriver.h（Apple SDK）](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/IOKit.framework/Versions/A/Headers/storage/IOBlockStorageDriver.h)，本机 ioreg 实测核对。

| C 常量 | key 字符串 | 单位/语义 |
|---|---|---|
| kIOBlockStorageDriverStatisticsKey | `"Statistics"` | 字典本身 |
| kIOBlockStorageDriverStatisticsBytesReadKey | `"Bytes (Read)"` | 累计读取字节数 |
| kIOBlockStorageDriverStatisticsBytesWrittenKey | `"Bytes (Write)"` | 累计写入字节数 |
| kIOBlockStorageDriverStatisticsReadsKey | `"Operations (Read)"` | 读操作数 |
| kIOBlockStorageDriverStatisticsWritesKey | `"Operations (Write)"` | 写操作数 |
| kIOBlockStorageDriverStatisticsReadErrorsKey | `"Errors (Read)"` | 读错误数 |
| kIOBlockStorageDriverStatisticsWriteErrorsKey | `"Errors (Write)"` | 写错误数 |
| kIOBlockStorageDriverStatisticsReadRetriesKey | `"Retries (Read)"` | 读重试数 |
| kIOBlockStorageDriverStatisticsWriteRetriesKey | `"Retries (Write)"` | 写重试数 |
| kIOBlockStorageDriverStatisticsTotalReadTimeKey | `"Total Time (Read)"` | **纳秒**（头文件原文 "number of nanoseconds spent performing reads"）；实测 2750644190662 ns ≈ 2750s ↔ 126.5GB 读 |
| kIOBlockStorageDriverStatisticsTotalWriteTimeKey | `"Total Time (Write)"` | **纳秒** |
| kIOBlockStorageDriverStatisticsLatentReadTimeKey | `"Latency Time (Read)"` | **纳秒**（累计延迟；实测常为 0，部分驱动不填） |
| kIOBlockStorageDriverStatisticsLatentWriteTimeKey | `"Latency Time (Write)"` | **纳秒** |

注意：Apple 文件系统层（APFS 容器）也挂在 IOBlockStorageDriver 族下且有同名 "Statistics" 字典但键完全不同
（"Bytes read from block device" 等）；按节点 `IOClass`/`IOProviderClass` 过滤，或只认上面 13 个 key。
实测（Apple SSD AP1024Z）：`Bytes (Read)=126536556544, Operations (Read)=13046945, Total Time (Read)=2750644190662, ...`

### 2.2 `IOBlockStorageDevice` 的 `"Device Characteristics"` 字典

常量：`kIOPropertyDeviceCharacteristicsKey = "Device Characteristics"`
（[IOStorageDeviceCharacteristics.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/IOKit.framework/Versions/A/Headers/storage/IOStorageDeviceCharacteristics.h)）

| C 常量 | key 字符串 | 语义/取值 | 验证 |
|---|---|---|---|
| kIOPropertyVendorNameKey | `"Vendor Name"` | 厂商名（Apple SSD 上为 `""`） | SDK+实测 |
| kIOPropertyProductNameKey | `"Product Name"` | 产品名，实测 `"APPLE SSD AP1024Z"` | SDK+实测 |
| kIOPropertyProductRevisionLevelKey | `"Product Revision Level"` | 固件版本，实测 `"591.40.3"` | SDK+实测 |
| kIOPropertyProductSerialNumberKey | `"Serial Number"` | 序列号字符串 | SDK+实测 |
| kIOPropertyMediumTypeKey | `"Medium Type"` | `"Solid State"` / `"Rotational"`（kIOPropertyMediumTypeSolidStateKey/RotationalKey） | SDK+实测 |
| kIOPropertyMediumRotationRateKey | `"Rotation Rate"` | 转速（rpm） | SDK |
| kIOPropertyBytesPerPhysicalSectorKey | `"Bytes per Physical Sector"` | 物理扇区字节数 | SDK |
| kIOPropertyPhysicalBlockSizeKey / kIOPropertyLogicalBlockSizeKey | `"Physical Block Size"` / `"Logical Block Size"` | 字节 | SDK |
| — | ⚠️ `"Product Id"` | 未在 IOStorageDeviceCharacteristics.h/ProtocolCharacteristics.h 中定义；USB/SCSI 设备常见但未验证 | 存疑 |

实测整字典（Apple SSD）：`{"Serial Number"="0ba01e4964d07a23","Medium Type"="Solid State","Product Name"="APPLE SSD AP1024Z","Vendor Name"="","Product Revision Level"="591.40.3"}`
同节点还有 `"NVMe SMART Capable" = Yes` 与 `"IOCFPlugInTypes" = {"AA0FA6F9-C2D6-457F-B10B-59A13253292F"="NVMeSMARTLib.plugin"}`（见 §3）。

### 2.3 `"Protocol Characteristics"` 字典

常量：`kIOPropertyProtocolCharacteristicsKey = "Protocol Characteristics"`
（[IOStorageProtocolCharacteristics.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/IOKit.framework/Versions/A/Headers/storage/IOStorageProtocolCharacteristics.h)）

| C 常量 | key 字符串 | 取值样例 |
|---|---|---|
| kIOPropertyPhysicalInterconnectTypeKey | `"Physical Interconnect"` | `"ATA"`,`"SATA"`,`"SAS"`,`"ATAPI"`,`"USB"`,`"FireWire"`,`"Secure Digital"`,`"SCSI Parallel Interface"`,`"Fibre Channel Interface"`,`"Virtual Interface"`,`"PCI"`；Apple Silicon 内置 NVMe 实测 `"Apple Fabric"` |
| kIOPropertyPhysicalInterconnectLocationKey | `"Physical Interconnect Location"` | `"Internal"`/`"External"`/`"Internal/External"`/`"File"`/`"RAM"`（常量 kIOPropertyInternalKey 等） |

（"Physical Interconnect"/"Physical Interconnect Location" 只在 Protocol Characteristics 下有官方定义；Device Characteristics 中未定义。）

### 2.4 `IOMedia` 节点 key（[IOMedia.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/IOKit.framework/Versions/A/Headers/storage/IOMedia.h) + 实测）

| C 常量 | key 字符串 | 类型/语义 | 实测 |
|---|---|---|---|
| kIOMediaSizeKey | `"Size"` | OSNumber，**字节**（1000555581440） | ✅ |
| kIOMediaWholeKey | `"Whole"` | OSBoolean，是否整盘 | ✅ |
| kIOBSDNameKey（IOBSD.h） | `"BSD Name"` | OSString，如 `"disk0"` | ✅ |
| kIOMediaRemovableKey | `"Removable"` | OSBoolean | ✅ |
| kIOMediaWritableKey | `"Writable"` | OSBoolean | ✅ |
| kIOMediaEjectableKey | `"Ejectable"` | OSBoolean | ✅ |
| kIOMediaLeafKey / kIOMediaOpenKey | `"Leaf"` / `"Open"` | OSBoolean | ✅ |
| kIOMediaPreferredBlockSizeKey | `"Preferred Block Size"` | OSNumber，字节（4096） | ✅ |
| kIOMediaContentKey / kIOMediaContentHintKey | `"Content"` / `"Content Hint"` | OSString，如 `"GUID_partition_scheme"` | ✅ |
| kIOMediaUUIDKey | `"UUID"` | OSString（分区 UUID，本机整盘节点无） | 头文件 |

---

## 3. NVMe SMART / ATA SMARTLib

### 3.1 IONVMeSMARTInterface（smartmontools os_darwin.h，基于 Xcode 私有头 NVMeSMARTLibExternal.h）

来源：[os_darwin.h](https://raw.githubusercontent.com/smartmontools/smartmontools/master/smartmontools/os_darwin.h)、[os_darwin.cpp](https://raw.githubusercontent.com/smartmontools/smartmontools/master/smartmontools/os_darwin.cpp)；UUID 与行为经本机实测。

| 常量 | 值 | 语义 |
|---|---|---|
| kIOPropertyNVMeSMARTCapableKey | `"NVMe SMART Capable"` | IOBlockStorageDevice 属性（实测 `Yes`），据此判断设备可读 SMART |
| kIONVMeSMARTUserClientTypeID | CFUUID 字节 `AA 0F A6 F9 C2 D6 45 7F B1 0B 59 A1 32 53 29 2F` = `AA0FA6F9-C2D6-457F-B10B-59A13253292F` | UserClient 类型；实测该 UUID 是 `IOCFPlugInTypes` 的键，值 `NVMeSMARTLib.plugin` |
| kIONVMeSMARTInterfaceID | CFUUID 字节 `CC D1 DB 19 FD 9A 4D AF BF 95 12 45 4B 23 0A B6` | QueryInterface 的接口 ID |

**`IONVMeSMARTInterface` vtable**（`IUNKNOWN_C_GUTS` = QueryInterface/AddRef/Release，后接 `UInt16 version; UInt16 revision;`）：

| 槽位 | 函数指针 | 签名 |
|---|---|---|
| 3 | `SMARTReadData` | `IOReturn (*)(void* intf, struct nvme_smart_log* NVMeSMARTData)` |
| 4 | `GetIdentifyData` | `IOReturn (*)(void* intf, struct nvme_id_ctrl* out, unsigned int ns)` |
| 5-6 | `reserved0, reserved1` | UInt64 |
| 7 | `GetLogPage` | `IOReturn (*)(void* intf, void* data, unsigned int logPageId, unsigned int numDWords)` —— numDWords 为 **0-based dwords 数** |
| 8+ | `reserved2..reserved19` | UInt64 |

**调用序列**（smartmontools 实现，本机 C 复现并跑通）：
`IOServiceGetMatchingServices("IOBlockStorageDevice")` → 找带 `NVMe SMART Capable` 的对象 →
`IOCreatePlugInInterfaceForService(dev, kIONVMeSMARTUserClientTypeID, kIOCFPlugInInterfaceID, &plugin, &score)` →
`(*plugin)->QueryInterface(plugin, CFUUIDGetUUIDBytes(kIONVMeSMARTInterfaceID), &ifp)` → `(*ifp)->SMARTReadData(ifp, buf)`。

**实测结果（macOS 27.2，普通用户，无 root）**：
`IOCreatePlugInInterfaceForService=0x0`、`QueryInterface=0x0`、`SMARTReadData=0x0`，
读到 `critical_warning=0, temperature=314K（开尔文）, available_spare=100%, percentage_used=3%`。
→ **读取不需要 root**（至少 macOS 12+ / Apple Silicon 如此；旧版 macOS/Intel 上 smartctl 曾需 root，返回 `kIOReturnNotPrivileged = 0xe00002c1`）⚠️ 版本差异。

**`nvme_smart_log`（512 字节，与 NVMe spec Log Page 02h 一致；smartmontools nvmecmds.h）**：

| 偏移 | 字段 | 类型 | 单位 |
|---|---|---|---|
| 0 | critical_warning | u8 | 位标志 |
| 1–2 | temperature | u16 LE | **开尔文** |
| 3 | avail_spare | u8 | % |
| 4 | spare_thresh | u8 | % |
| 5 | percent_used | u8 | %（寿命已用） |
| 6–31 | rsvd6[26] | | |
| 32–47 | data_units_read | u128 LE | 1 unit = 512000 字节（1000×512B） |
| 48–63 | data_units_written | u128 LE | 同上 |
| 64–79 | host_reads | u128 LE | 命令数 |
| 80–95 | host_writes | u128 LE | 命令数 |
| 96–111 | ctrl_busy_time | u128 LE | 分钟 |
| 112–127 | power_cycles | u128 LE | |
| 128–143 | power_on_hours | u128 LE | 小时 |
| 144–159 | unsafe_shutdowns | u128 LE | |
| 160–175 | media_errors | u128 LE | |
| 176–191 | num_err_log_entries | u128 LE | |
| 192–195 | warning_temp_time | u32 LE | 分钟 |
| 196–199 | critical_comp_time | u32 LE | 分钟 |
| 200–215 | temp_sensor[8] | u16 LE ×8 | 开尔文 |
| 216–231 | thm_temp1/2_trans_count, thm_temp1/2_total_time | u32 ×4 | |
| 232–511 | rsvd232[280] | | |

### 3.2 ATA SMARTLib（<IOKit/storage/ata/ATASMARTLib.h>）

来源：[ATASMARTLib.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/IOKit.framework/Versions/A/Headers/storage/ata/ATASMARTLib.h)

| 常量 | 值 |
|---|---|
| kIOPropertySMARTCapableKey | `"SMART Capable"` |
| kIOATASMARTLibFactoryID | `5E659F92-20D3-11D6-BDB5-0003935A76B2`（字节 5E 65 9F 92 20 D3 11 D6 BD B5 00 03 93 5A 76 B2） |
| kIOATASMARTUserClientTypeID | `24514B7A-2804-11D6-8A02-003065704866` |
| kIOATASMARTInterfaceID | `08ABE21C-20D4-11D6-8DF6-0003935A76B2` |

**`IOATASMARTInterface` vtable 顺序**（同样走 IOCreatePlugInInterfaceForService + QueryInterface，非编号 selector）：
`SMARTEnableDisableOperations(BOOL enable)`、`SMARTEnableDisableAutosave(BOOL)`、`SMARTReturnStatus(BOOL* exceeded)`、
`SMARTExecuteOffLineImmediate(BOOL)`、`SMARTReadData(ATASMARTData*)`、`SMARTValidateReadData(ATASMARTData*)`、
`SMARTReadDataThresholds(ATASMARTDataThresholds*)`、`SMARTReadLogDirectory(ATASMARTLogDirectory*)`、
`SMARTReadLogAtAddress(UInt32 logOffset, void* buf, UInt32 size)`、`SMARTWriteLogAtAddress(...)`、
`GetATAIdentifyData(void* buf, UInt32 inSize, UInt32* outSize)`。

结构体：`ATASMARTData`（512B：`vendorSpecific1[362]`、`offLineDataCollectionStatus`@362、`selfTestExecutionStatus`@363、
`secondsToCompleteOffLineActivity[2]`、…、`shortTestPollingInterval`/`extendedTestPollingInterval`（分钟）、`checksum`@511）、
`ATASMARTDataThresholds`（512B）、`ATASMARTLogDirectory`、`ATASMARTLogEntry`。
SMART 属性（ID/flags/current/worst/raw[6]）就在 `vendorSpecific1` 的偏移 2 起（每 12B 一条，共 30 条）⚠️ 需自行按 ATA 规范解析。

⚠️ 存疑：用户提到的 `kIOPropertySMARTUserClientTypeID`、`kIOSMARTInterfaceID`、`SMARTExecuteCommand`、
`kNVMeSMARTGetLogPage` 等名称未出现在官方 SDK 头或 smartmontools 中（可能是私有头 NVMeSMARTLibExternal.h
另一版本/第三方转载的命名）。**没有"selector 编号"一说**：这些接口是 COM 风格 vtable 函数指针
（`IOCFPlugInInterface`），不是 `IOConnectCallScalarMethod(selector)` 的外部方法表。

---

## 4. WiFi（CoreWLAN）

来源：[CWInterface.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/CoreWLAN.framework/Versions/A/Headers/CWInterface.h)、
[CWWiFiClient.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/CoreWLAN.framework/Versions/A/Headers/CWWiFiClient.h)、
CoreWLANTypes.h、本机 Xcode SDK 头 + 编译实测。

### 4.1 对象与方法（ObjC 签名；objc_msgSend 可直接调用）

| 类 | 消息 | 返回 |
|---|---|---|
| CWWiFiClient | `+sharedWiFiClient` | `CWWiFiClient*` |
| CWWiFiClient | `-interface` / `-interfaceWithName:(NSString*)` | `CWInterface* _Nullable` |
| CWWiFiClient | `-interfaces` | `NSArray<CWInterface*>* _Nullable` |
| CWWiFiClient | `+interfaceNames` | `NSArray<NSString*>* _Nullable` |
| CWInterface | `-interfaceName` | `NSString*`（如 `"en0"`） |
| CWInterface | `-powerOn` | `BOOL` |
| CWInterface | `-ssid` / `-ssidData` | `NSString*` / `NSData*`，**无定位权限时返回 nil**（macOS 27 实测） |
| CWInterface | `-bssid` | `NSString*`，同上受定位权限限制 |
| CWInterface | `-rssiValue` / `-noiseMeasurement` | `NSInteger`（dBm，负值；实测 -61 / -91） |
| CWInterface | `-transmitRate` | `double`（Mbps；实测 864.0） |
| CWInterface | `-transmitPower` | `NSInteger`（实测 1496；单位 ⚠️ 存疑，头文件称 mW） |
| CWInterface | `-wlanChannel` | `CWChannel*` |
| CWInterface | `-security` | `CWSecurity`（NSInteger） |
| CWInterface | `-activePHYMode` | `CWPHYMode`（NSInteger） |
| CWInterface | `-interfaceMode` | `CWInterfaceMode`（NSInteger） |
| CWInterface | `-countryCode` | `NSString*`（实测 nil） |
| CWInterface | `-hardwareAddress` | `NSString*`（MAC，无需权限；实测可读） |
| CWInterface | `-serviceActive` | `BOOL` |
| CWChannel | `-channelNumber` | `NSInteger` |
| CWChannel | `-channelBand` | `CWChannelBand`（**现代属性名是 channelBand，不是 band**） |
| CWChannel | `-channelWidth` | `CWChannelWidth`（**不是 width**） |

`+interface`/`+interfaceWithName:` 在 10.10 起废弃，请用 CWWiFiClient。
⚠️ `networkName()`：现代 SDK 的 CWInterface/CWNetwork 都没有该方法（只有 `scanForNetworksWithName:` 参数同名）；
objc2-core-wlan 文档中的 `networkName` 需另行核实，**存疑**。

### 4.2 枚举值（macOS 27 SDK CoreWLANTypes.h 逐行核对；10.11 旧 SDK 无 WPA3/6GHz/11ax/11be）

```
CWPHYMode(NSInteger):        None=0, 11a=1, 11b=2, 11g=3, 11n=4, 11ac=5, 11ax=6, 11be=7
CWInterfaceMode(NSInteger):  None=0, Station=1, IBSS=2, HostAP=3
CWSecurity(NSInteger):       None=0, WEP=1, WPAPersonal=2, WPAPersonalMixed=3, WPA2Personal=4,
                             Personal=5, DynamicWEP=6, WPAEnterprise=7, WPAEnterpriseMixed=8,
                             WPA2Enterprise=9, Enterprise=10, WPA3Personal=11, WPA3Enterprise=12,
                             WPA3Transition=13, OWE=14, OWETransition=15, Unknown=NSIntegerMax
CWChannelWidth(NSInteger):   Unknown=0, 20MHz=1, 40MHz=2, 80MHz=3, 160MHz=4
CWChannelBand(NSInteger):    Unknown=0, 2GHz=1, 5GHz=2, 6GHz=3
CWEventType(NSInteger):      None=0, PowerDidChange=1, SSIDDidChange=2, BSSIDDidChange=3,
                             CountryCodeDidChange=4, LinkDidChange=5, LinkQualityDidChange=6,
                             ModeDidChange=7, ScanCacheUpdated=8, BtCoexStats=9, Unknown=NSIntegerMax
CWErr(NSInteger):            kCWNoErr=0, kCWEAPOLErr=1, kCWInvalidParameterErr=-3900 … kCWErr=-3931
```
（用户问的 `CWSecurityModeOpen=1`、`CWChannelBand2GHz=1`：前者是 10.6 时代废弃枚举 `CWSecurityMode`，现代用
`CWSecurity`（Open 对应 `kCWSecurityNone=0`）；`kCWChannelBand2GHz=1` ✅ 正确。）

### 4.3 实测与权限

- 本机实测（WPA3 网络）：`security=11`(WPA3Personal)、`activePHYMode=6`(11ax)、`channelNumber=36`、`channelBand=2`(5GHz)、`channelWidth=4`(160MHz)、`interfaceMode=1`(Station)。
- **ssid()/bssid() 需要定位权限**（macOS 10.15+ 限制，macOS 14+ 更严），无权限返回 nil（实测）；rssi/noise/速率/信道/国家码/硬件地址不需要。Rust 侧建议把 ssid/bssid 当可失败路径。
- 不需要 root。
- objc 裸调用可行性：✅ 所有方法都是普通 ObjC 消息；arm64 下 `NSInteger`=long、`double` 返回无需 `objc_msgSend_fpret`（x86_64 才需要）；也可用 `objc2` / `objc2-core-wlan` crate。

---

## 5. 显示器（CoreGraphics / CoreVideo / IOKit）

来源：[CGDirectDisplay.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/CoreGraphics.framework/Versions/A/Headers/CGDirectDisplay.h)、
[CVBase.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/CoreVideo.framework/Versions/A/Headers/CVBase.h)、
[CVDisplayLink.h](https://raw.githubusercontent.com/phracker/MacOSX-SDKs/master/MacOSX10.11.sdk/System/Library/Frameworks/CoreVideo.framework/Versions/A/Headers/CVDisplayLink.h)，本机实测。

| API | 签名 | 语义 | 实测（Mac14,9，ProMotion 内建屏 + 外接 4K） |
|---|---|---|---|
| `CGGetActiveDisplayList` | `(uint32_t max, CGDirectDisplayID* ids, uint32_t* count) -> CGError` | CGDirectDisplayID=`uint32_t` | err=0, n=2 |
| `CGMainDisplayID` | `() -> CGDirectDisplayID` | 主屏 | ✅ |
| `CGDisplayIsBuiltin` | `(id) -> bool` | 是否内建 | 内建=1、外接=0 ✅ |
| `CGDisplayVendorNumber/ModelNumber/SerialNumber` | `(id) -> uint32_t` | 对应 EDID 厂商(2B EISA)/产品(2B LE)/序列(4B LE) | 内建 vendor=0x610("APP")、model=0xa04f、serial=0xfd626d62；外接 vendor=0x22f6 ✅ |
| `CGDisplayPixelsWide/High` | `(id) -> size_t` | **像素** | 内建 3600×2338（HiDPI） ✅ |
| `CGDisplayCopyDisplayMode` | `(id) -> CGDisplayModeRef` | 当前模式 | ✅ |
| `CGDisplayModeGetWidth/GetHeight` | `(mode) -> size_t` | 逻辑点 | 1800×1169 ✅ |
| `CGDisplayModeGetPixelWidth/GetPixelHeight` | `(mode) -> size_t` | 物理像素 | 3600×2338 ✅ |
| `CGDisplayModeGetRefreshRate` | `(mode) -> double` | **Hz**；ProMotion 当前模式实测 **120.000**（不是 0 也不是 60）；外接 60.000 | ✅ |
| `CGDisplayCopyAllDisplayModes` | `(id, CFDictionaryRef options) -> CFArrayRef` | **现代 SDK 是 2 参**（旧 3 参带 countOut 的写法会编译失败） | 内建屏 60 个模式：120.000/60.000/59.940/48.000/47.950 等 |
| `CGDisplayModeGetIOFlags` | `(mode) -> uint32_t` | IODisplay 模式 flags | 0x3 常见，HiDPI 模式 0x2000003 ✅ |
| `CGDisplayModeCopyPixelEncoding` | `(mode) -> CFStringRef` | 已 deprecated，实测 `"--------RRRRRRRRGGGGGGGGBBBBBBBB"` | ✅ |
| `CGDisplayRotation` | `(id) -> double` | 度 | 0.0 ✅ |

**CVDisplayLink**：
- `CVDisplayLinkCreateWithCGDisplay(CGDirectDisplayID, CVDisplayLinkRef*) -> CVReturn`
- `CVDisplayLinkGetNominalOutputVideoRefreshPeriod(link) -> CVTime{ int64_t timeValue; int32_t timeScale; int32_t flags; }`
- `kCVTimeIsIndefinite = 1 << 0`（=1）（CVBase.h enum）。
- **实测 ProMotion 屏**：`timeValue=400108, timeScale=24000000, flags=0` → 周期 1/59.98 s，即 **返回 ≈59.98Hz，不是 indefinite 也不是 120**（ProMotion 自适应刷新下 nominal 视作 60Hz 基准）。⚠️ 不同 macOS/机型可能有差异。
- `CVDisplayLinkGetActualOutputVideoRefreshPeriod(link) -> double`（Hz；link 未启动实测 0.0）。

**IODisplayConnect / EDID**：
- Intel Mac 经典路径：`IODisplayConnect` 节点属性 `"IODisplayEDID"`（CFData，128B+ EDID）、`"IODisplayIsDigital"`（⚠️ 存疑：类型常见为 OSBoolean/OSNumber，本机无法验证）。
- **Apple Silicon 实测**：没有 `IODisplayConnect` 实例；EDID 以 `"EDID"`（CFData）挂在 DCP 显示传输链
  （`IOPortTransportStateDisplayPort`/`...CIO`/`...PCIe`）节点，同级还有 `"EDID UUID"`、`"HasHDMILegacyEDID"`。
- EDID 解析：bytes 0–7 头（`00 FF FF FF FF FF FF 00`）、8–9 厂商 EISA 三字母、10–11 产品码（LE）、12–15 序列号（LE）、
  54 起 4×18B descriptor（0xFC 块为产品名 ASCII，实测外接屏 `"MateView"`；0xFF 块为序列号字符串）。
  `CGDisplayVendorNumber/ModelNumber/SerialNumber` 与 EDID 8–15 字节对应（CG 文档/EDID 解析库一致）。
- 显示器名称：`IODisplayCreateInfoDictionary(service, kIODisplayOnlyPreferredName)` 得 `"DisplayProductName"`
  本地化字典（⚠️ key 字符串与 Intel 实测来源待核）。Rust 可直接解析 EDID。

---

## 6. 电池

### 6.1 `AppleSmartBattery` 注册表（本机 Apple Silicon 实测 59 keys，非 root 可读）

顶层标量（实测值 / 单位）：

| key | 实测值 | 类型 | 语义与单位 |
|---|---|---|---|
| `"Voltage"` | 12605 | OSNumber | **mV** |
| `"AppleRawBatteryVoltage"` | 12605 | OSNumber | mV（未换算原始值） |
| `"Amperage"` | 0 | OSNumber(有符号) | **mA**；负=放电、正=充电（⚠️ 方向约定按 Intel 传统，本机充电态未抓到） |
| `"InstantAmperage"` | 0 | OSNumber(有符号) | mA，瞬时电流 |
| `"CurrentCapacity"` | 100 | OSNumber | **百分比**（Apple Silicon；mAh 值在 BatteryData） |
| `"MaxCapacity"` | 100 | OSNumber | **百分比** |
| `"CycleCount"` | 187 | OSNumber | 循环次数 |
| `"TimeRemaining"` | 65535 | OSNumber | **分钟**；65535=无效（接 AC 时） |
| `"AvgTimeToEmpty"` / `"AvgTimeToFull"` | 65535 | OSNumber | 分钟；65535=无效 |
| `"FullyCharged"` / `"IsCharging"` / `"ExternalConnected"` / `"BatteryInstalled"` / `"AtCriticalLevel"` | Yes/No | OSBoolean | 布尔 |
| `"Serial"` | "F8Y2535H7U9Q1LTA5" | OSString | 电池序列号（⚠️ Intel 机器常见 `BatterySerialNumber`；systeminformation 两者都试） |
| `"DeviceName"` | "bq40z651" | OSString | 电池芯片/名称 |
| `"DesignCycleCount9C"` | 1000 | OSNumber | 设计循环寿命 |
| `"built-in"` | Yes | OSBoolean | 内置电池 |
| `"AppleRawExternalConnected"`、`"ExternalChargeCapable"`、`"BootVoltage"`、`"ChargerCount"`、`"BankCount"`、`"CellID"`、`"PackID"`、`"Location"`、`"FeatureFlags"`、`"KioskMode"`、`"CarrierMode"`、`"UpdateTime"` 等 | | | 杂项 |

`"BatteryData"` 子字典（实测；**mAh 单位**为主）：
`"CurrentCapacity"=100(%)`, `"MaxCapacity"=100(%)`, `"AppleRawMaxCapacity"=4908(mAh)`,
`"AppleRawCurrentCapacity"=4908(mAh)`, `"DesignCapacity"=6075(mAh)`, `"NominalChargeCapacity"=5058(mAh)`,
`"FullChargeCapacity"=4908(mAh)`, `"RemainingCapacity"=4908(mAh)`, `"FullyCharged"=1`, `"BatteryPower"=0`,
`"AvgTimeToEmpty"=65535`, `"AbsoluteCapacity"`, `"TrueRemainingCapacity"`。

其它子字典：`"AdapterDetails"`（`Watts=100`、`Current=5000`(mA)、`AdapterVoltage=20000`(mV)、`FamilyCode`…）、
`"ChargerData"`（`IsCharging`、`NotChargingReason`）、`"PowerTelemetryData"`、`"CellData"`、`"FedDetails"`、`"PortControllerInfo"`。

⚠️ 本机不存在的（Intel Mac 传统 key，无法验证）：`Temperature`（传闻 0.1K 或 °C×100，**存疑**）、`ManufactureDate`（BCD）、
`DesignVoltage`、`BatterySerialNumber`、`Manufacturer`（多为 `ManufacturerData` blob）、顶层 `DesignCapacity`/
`AppleRawMaxCapacity`/`AppleRawCurrentCapacity`（Apple Silicon 在 BatteryData 内）。`AppleRawBattery` 类本机不存在。
来源：本机 `ioreg -l -c AppleSmartBattery -d 1`；交叉验证 [systeminformation battery.js](https://raw.githubusercontent.com/sebhildebrandt/systeminformation/master/lib/battery.js)
（macOS 端执行 `ioreg -n AppleSmartBattery -r | egrep "CycleCount|IsCharging|DesignCapacity|MaxCapacity|CurrentCapacity|DeviceName|BatterySerialNumber|Serial|TimeRemaining|Voltage|BatteryData|NominalChargeCapacity"`）。

### 6.2 IOPowerSources（<IOKit/ps/IOPowerSources.h>、<IOKit/ps/IOPSKeys.h>；**无需任何权限**）

API 签名：
- `CFTypeRef IOPSCopyPowerSourcesInfo(void);`
- `CFArrayRef IOPSCopyPowerSourcesList(CFTypeRef blob);`
- `CFStringRef IOPSGetProvidingPowerSourceType(CFTypeRef blob);` → `kIOPSACPowerValue`/`kIOPSBatteryPowerValue`/`kIOPSOffLineValue`
- `CFDictionaryRef IOPSGetPowerSourceDescription(CFTypeRef blob, CFTypeRef ps);`
- `CFTimeInterval IOPSGetTimeRemainingEstimate(void);` → `kIOPSTimeRemainingUnknown = -1.0`、`kIOPSTimeRemainingUnlimited = -2.0`（接 AC 实测 -2.0）

key 字符串（IOPSKeys.h 宏定义；⚠️ 常量是 C 字符串，FFI 中要 `CFSTR()`）：

| C 常量 | 字符串 | 语义 |
|---|---|---|
| kIOPSPowerSourceIDKey | `"Power Source ID"` | id |
| kIOPSPowerSourceStateKey | `"Power Source State"` | 值：`"AC Power"`/`"Battery Power"`/`"Off Line"` |
| kIOPSCurrentCapacityKey | `"Current Capacity"` | 当前容量（**百分比**，实测 100） |
| kIOPSMaxCapacityKey | `"Max Capacity"` | 最大容量（百分比，实测 100） |
| kIOPSDesignCapacityKey | `"DesignCapacity"`（**无空格**） | 设计容量 |
| kIOPSNominalCapacityKey | `"Nominal Capacity"` | 标称容量 |
| kIOPSTimeToEmptyKey | `"Time to Empty"` | 分钟（实测 0） |
| kIOPSTimeToFullChargeKey | `"Time to Full Charge"` | 分钟（实测 0） |
| kIOPSIsChargingKey | `"Is Charging"` | bool（实测 0） |
| kIOPSIsChargedKey | `"Is Charged"` | bool |
| kIOPSIsFinishingChargeKey | `"Is Finishing Charge"` | bool |
| kIOPSIsPresentKey | `"Is Present"` | bool |
| kIOPSVoltageKey / kIOPSCurrentKey / kIOPSTemperatureKey | `"Voltage"` / `"Current"` / `"Temperature"` | ⚠️ 单位未在头文件中写明（存疑） |
| kIOPSNameKey / kIOPSTypeKey / kIOPSTransportTypeKey | `"Name"` / `"Type"` / `"Transport Type"` | 实测 `"InternalBattery-0"` / `"InternalBattery"` / `"Internal"` |
| kIOPSHardwareSerialNumberKey | `"Hardware Serial Number"` | 实测可得电池序列 |
| kIOPSBatteryHealthKey | `"BatteryHealth"` | 值 `"Good"/"Fair"/"Poor"`（**没有** kIOPSHealthKey 这个名字） |
| kIOPSBatteryHealthConditionKey | `"BatteryHealthCondition"` | 值 `"Check Battery"`/`"Permanent Battery Failure"` 等 |
| kIOPSHealthConfidenceKey / kIOPSMaxErrKey | `"HealthConfidence"` / `"MaxErr"` | |
| kIOPSPowerAdapter*Key | `"AdapterID"`,`"Watts"`(W),`"Current"`,`"SerialNumber"`,`"FamilyCode"`,`"Source"`,`"AdapterRevision"` | 适配器字典 |

实测 IOPS 描述字典出现的 key（macOS 27）：`"Is Charged","Current","Is Charging","Name","Type","Time to Full Charge","Power Source ID","Time to Empty","Transport Type","Hardware Serial Number","Battery Provides Time Remaining","DesignCycleCount","Current Capacity","Max Capacity","Power Source State","LPM Active","Is Present"`。
⚠️ 新增 key `"Battery Provides Time Remaining"`、`"LPM Active"`、`"DesignCycleCount"` 不在 10.11 IOPSKeys.h；
`kIOPSCycleCountKey` 不存在（循环数请读 IORegistry `AppleSmartBattery.CycleCount`）。

---

## 附：权限总结（本机实测）

| 数据 | root 需求 |
|---|---|
| GPU PerformanceStatistics | 否（普通用户 ioreg/IOKit 均可） |
| IOBlockStorageDriver Statistics / Device Characteristics / IOMedia | 否 |
| NVMe SMART（IONVMeSMARTInterface plugin） | **否**（macOS 27.2 实测通过；旧系统/Intel ⚠️ 存疑） |
| AppleSmartBattery / IOPowerSources | 否 |
| CoreWLAN rssi/noise/channel 等 | 否 |
| CoreWLAN ssid/bssid | 需定位权限（无则 nil），非 root |

失败返回码：`kIOReturnNotPrivileged = 0xe00002c1`（IOKit/IOReturn.h）；未观测到（本机全部走通）。

---

## 实测勘误(2026-10-12,macOS Disk provider 实现时发现)

1. **§3.1 的 IONVMeSMARTInterface vtable 槽位号表不准确**:`IUNKNOWN_C_GUTS`
   (CFPlugInCOM.h 权威原文)含 `void *_reserved` 首字段,且 `version`/`revision`
   是数据字段(带 4 字节对齐填充),`SMARTReadData` 实际位于第 8 个指针位而非第 3。
   **按索引调用会打错函数**;实现应改用具名 `#[repr(C)]` 结构体(VtableHead
   嵌套前缀)取函数指针,零索引猜测。
2. **`IOCreatePlugInInterfaceForService` 的两个 UUID 参数极易传反**:
   `pluginType` = user client 类型 UUID(kIONVMeSMARTUserClientTypeID)在前、
   `interfaceType` = kIOCFPlugInInterfaceID 在后。传反不崩溃,只静默返回
   `SMARTReadData` 失败(Error)——典型静默坑。
3. **挂载的 DMG 是 `IOBlockStorageDevice` 的子类**(AppleDiskImageDevice,
   Protocol Characteristics 为 "Virtual Interface"/"File",≈ Windows 的
   FileBackedVirtual),会进磁盘枚举;SMC SSD 温度(TH0x)的"内置单盘"归属
   判定应只计 `Physical Interconnect Location == "Internal"` 的盘。
4. **空 SD 读卡器**(AppleSDXCBlockStorageDevice)无整盘 IOMedia → 按
   "无介质不算盘"跳过;APFS 层同名 `"Statistics"` 字典按父节点配对
   IOBlockStorageDriver + `"Bytes (Read)"` 键存在性双重判据排除,读不错对象。
