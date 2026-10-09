/*
 * hwprobe.h — hwprobe.dll 的 C ABI 契约(与 crates/hwprobe-dll/src/lib.rs 严格对应)
 * =============================================================================
 *
 *  hwprobe —— 类 HWiNFO 硬件信息与传感器监测库(Windows 10/11 x64)
 *
 *  提供 CPU / 内存 / GPU / 硬盘 / 网卡 / 电池 / 主板(含 BIOS)/ 显示器 /
 *  惯性传感器(IMU)的静态识别与实时动态传感器数据。任何能调用 C ABI 的
 *  语言(C/C++/C#/Python/AHK…)均可使用;免安装,把 hwprobe.dll 放在程序
 *  同目录即可。
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  一、数据模型:三层树
 * ─────────────────────────────────────────────────────────────────────────────
 *      类别 Category(9 种固定分类) → 设备 Device(实例) → 传感器 Sensor(数据点)
 *
 *      例如:CPU(类别) → "i5-10600KF"(设备) → "Core #0 Usage"(传感器)
 *           Disk(类别) → "Samsung 980"(设备) → "Temperature"(传感器)
 *
 *  所有动态数据都用三个 uint32 下标寻址:(category, device, sensor)。
 *  下标从 0 开始,同一进程生命周期内保持稳定(设备不会中途增删)。
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  二、两种取数方式
 * ─────────────────────────────────────────────────────────────────────────────
 *  1. 订阅推送(推荐,ABI 2 起):
 *     用 hwprobe_subscribe() 注册一个回调,DLL 在内部线程上按节拍把
 *     【全部动态传感器的快照】主动推给回调。调用方不需要任何轮询循环,
 *     也不会因为高频刷新把系统负载拉高 —— 采集始终由 DLL 按自己的节拍做。
 *
 *  2. 拉取(ABI 1 起可用):
 *     调用方自己循环调用 hwprobe_read_sensor()。读的是 DLL 内部缓存,
 *     后台线程自动按 TTL 刷新,单次读值毫秒级返回、不触发实际采集。
 *     频繁调用读值不会明显增加负载;但请不要高频调 hwprobe_refresh()。
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  三、基本规则(先读这一段)
 * ─────────────────────────────────────────────────────────────────────────────
 *  - 所有导出函数为 extern "system"(x64 下与 cdecl 相同),可多线程并发调用;
 *  - 返回值 i32 是状态码 HWPROBE_*(0 = HWPROBE_OK 表示成功);
 *  - 单个传感器读不到数据【不会】导致调用失败:读值结构体里每个传感器
 *    独立携带 status(如 NO_DRIVER / NOT_SUPPORTED),一个数据源故障
 *    不影响其他字段 —— 这是本库的设计原则:绝不给假数据,逐项降级;
 *  - 字符串一律 UTF-8、NUL 结尾。在中文 Windows 上与 GUI API 交互时
 *    注意 UTF-8 → UTF-16 转换,不要按 GBK/ANSI 解码;
 *  - 所有结构体首字段 size:DLL 会写入 sizeof(结构体);调用方分配前
 *    也应预填同样值,供未来版本做兼容校验;
 *  - hwprobe_init() 必须最先调用(幂等),进程退出前调用 hwprobe_shutdown()。
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  四、快速上手 —— 订阅推送模式(推荐)
 * ─────────────────────────────────────────────────────────────────────────────
 *  #include "hwprobe.h"
 *
 *  static void on_batch(const hwprobe_stream_batch *b, void *ud)
 *  {
 *      —— 回调运行在 DLL 内部线程上:只做拷贝/入队,立即返回!
 *         不要在这里阻塞、刷新或关闭库 ——
 *      for (uint32_t i = 0; i < b->count; i++) {
 *          const hwprobe_stream_item *it = &b->items[i];
 *          if (it->status != HWPROBE_OK) continue;          只关心正常读值
 *          if (it->value_type == HWPROBE_VTYPE_F64)
 *              printf("%u.%u.%u = %.1f\n",
 *                     it->category, it->device, it->sensor, it->value.f64);
 *      }
 *  }
 *
 *  int main(void)
 *  {
 *      hwprobe_init();                            ← 1. 初始化(枚举硬件+启动采集)
 *
 *      (可选)枚举设备与传感器,建立 三元键 → 名称 的映射表:
 *      推送的 item 只带下标,展示名称要在枚举阶段自己存好。
 *
 *      uint64_t h = 0;
 *      hwprobe_subscribe(on_batch, NULL, 1000,    ← 2. 每 1000ms 推一批
 *                        HWPROBE_ALL_CATEGORIES, &h);
 *
 *      ...                                        ← 3. 主循环直接使用回调拷出来的
 *                                                        数据,无需任何轮询 …
 *
 *      hwprobe_unsubscribe(h);                    ← 4. 退订(可省略,shutdown 兜底)
 *      hwprobe_shutdown();                        ← 5. 释放
 *      return 0;
 *  }
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  五、快速上手 —— 拉取模式
 * ─────────────────────────────────────────────────────────────────────────────
 *  hwprobe_init();
 *  Sleep(1500);                       ← 等首轮刷新,否则动态值处于 WAITING
 *
 *  for (;;) {
 *      hwprobe_reading rd;
 *      if (hwprobe_read_sensor(HWPROBE_CAT_CPU, 0, 0, &rd) == HWPROBE_OK
 *          && rd.status == HWPROBE_OK
 *          && rd.value_type == HWPROBE_VTYPE_F64)
 *          printf("CPU Total = %.1f %%\n", rd.value.f64);
 *      Sleep(1000);                   ← 读内部缓存,不触发采集,放心循环
 *  }
 *  hwprobe_shutdown();
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  六、内核驱动增强层(可选)
 * ─────────────────────────────────────────────────────────────────────────────
 *  CPU 核心温度/封装功耗、主板风扇转速/电压等必须经内核驱动读取,普通
 *  用户态 API 拿不到。调用 hwprobe_setup_driver() 可一键静默安装官方
 *  PawnIO 驱动(弹 UAC,装一次即可);未装驱动时这些传感器报
 *  NO_DRIVER / NOT_SUPPORTED,其余功能完全正常。
 *  注意:访问 PawnIO 设备需要管理员权限;环境变量 HWPROBE_NO_DRIVER=1
 *  可完全跳过驱动加载。
 *
 * ─────────────────────────────────────────────────────────────────────────────
 *  七、其他语言绑定注意
 * ─────────────────────────────────────────────────────────────────────────────
 *  - hwprobe_value 是 96 字节的 C union(f64/i64/u64/bool/char[96] 共享
 *    同一起始偏移)。C# 映射时必须用显式重叠布局(FieldOffset(0))或
 *    fixed byte[96] 缓冲,不能写成一片 Sequential 字段,否则取值错位;
 *  - Python ctypes:Structure + Union 对应即可,注意 u64 字段的对齐;
 *  - 回调(订阅推送)在 C# 里是委托:订阅期间必须保持委托引用(存到
 *    字段),被 GC 回收后原生回调会崩;Python ctypes 同理,回调对象要
 *    存变量保活。
 */
#ifndef HWPROBE_H
#define HWPROBE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---------------------------------------------------------------------------
 * 版本与全局常量
 * ------------------------------------------------------------------------- */

/* ABI 版本:1 = 仅拉取模式;2 = 增加订阅推送(hwprobe_subscribe 等);
 * 3 = 新增 Display(7)/IMU(8)类别;4 = 新增 hwprobe_set_usage_mode
 * (占用口径切换)与 GPU 分引擎传感器 util_eng_*(见下)。
 * 新增导出/状态码会递增此值,已有声明保持兼容。
 * 可用 hwprobe_abi_version() 在运行时查询 DLL 实际版本。 */
#define HWPROBE_ABI_VERSION 4

/* “全部类别”通配值:传给 hwprobe_refresh 表示刷新所有类别;
 * 传给 hwprobe_subscribe 的 category_mask 表示订阅所有类别。 */
#define HWPROBE_ALL_CATEGORIES 0xFFFFFFFFu

/* ---------------------------------------------------------------------------
 * 状态码 —— 同时是【每个导出函数的返回值】和【每个传感器读值的 status 字段】
 * ------------------------------------------------------------------------- */
enum {
    HWPROBE_OK             = 0,   /* 成功 */
    HWPROBE_WAITING        = 1,   /* 动态传感器还没等到第一次后台刷新(瞬时状态,稍后再读) */
    HWPROBE_NOT_INITIALIZED= 2,   /* 还没调用 hwprobe_init() */
    HWPROBE_INIT_FAILED    = 3,   /* 初始化失败(详见 hwprobe_last_error) */
    HWPROBE_INVALID_ARG    = 4,   /* 参数非法(NULL 指针 / mask 为 0 等) */
    HWPROBE_BAD_CATEGORY   = 5,   /* 类别下标越界 */
    HWPROBE_BAD_DEVICE     = 6,   /* 设备下标越界 */
    HWPROBE_BAD_SENSOR     = 7,   /* 传感器下标越界 */
    HWPROBE_NO_DRIVER      = 8,   /* 该传感器需要内核驱动(温度/风扇/电压),驱动不可用 */
    HWPROBE_NOT_SUPPORTED  = 9,   /* 该设备/通道不支持此传感器(如 BIOS 关闭的风扇通道) */
    HWPROBE_ERROR          = 10,  /* 一般性错误(采集失败等) */
    HWPROBE_BUFFER_TOO_SMALL = 11,/* 输出缓冲区不够大(*len 会返回所需大小) */
    HWPROBE_UNKNOWN        = 12,  /* 未知错误 */
    HWPROBE_BAD_HANDLE     = 13,  /* 订阅句柄不存在(或已取消) */
};

/* ---------------------------------------------------------------------------
 * 类别索引 —— hwprobe_get_category_info / 各接口的第一个 category 参数
 * ------------------------------------------------------------------------- */
enum {
    HWPROBE_CAT_CPU          = 0,  /* 处理器:占用/逐核占用/频率(/温度/功耗,需驱动) */
    HWPROBE_CAT_MEMORY       = 1,  /* 物理内存:已用/可用/提交 */
    HWPROBE_CAT_GPU          = 2,  /* 显卡:每块 GPU 一个设备(温度/功耗/风扇/频率/显存) */
    HWPROBE_CAT_DISK         = 3,  /* 硬盘:每块物理盘一个设备(温度/健康度/活动率/吞吐) */
    HWPROBE_CAT_NETWORK      = 4,  /* 网卡:每张适配器一个设备(链路/速率/收发吞吐/驱动
                                      版本);WiFi 适配器另有 SSID/BSSID/频段/信道/
                                      信号/RSSI/协商速率/认证加密/射频开关,以及网卡
                                      支持的 802.11 制式 */
    HWPROBE_CAT_BATTERY      = 5,  /* 电池:电量/电压/充放状态(台式机无此类别) */
    HWPROBE_CAT_MOTHERBOARD  = 6,  /* 主板:BIOS/系统/板信息 + 板载温度/风扇/电压(需驱动);
                                      掌机(MSI Claw 等)另有 "Embedded Controller (EC)"
                                      第二设备:风扇转速/CPU·GPU 温度/性能模式/充电上限 */
    HWPROBE_CAT_DISPLAY      = 7,  /* 显示器/面板:分辨率/刷新率/连接类型/触屏(ABI 3 起) */
    HWPROBE_CAT_IMU          = 8,  /* 惯性传感器:加速度计(g)/陀螺仪(°/s)(ABI 3 起) */
    HWPROBE_CATEGORY_COUNT   = 9,  /* 类别总数 */
};

/* 传感器种类:静态 = 识别信息,probe 时取一次,值不随时间变化;
 *             动态 = 实时数据,由后台线程持续刷新/推送。 */
enum {
    HWPROBE_SENSOR_STATIC  = 0,
    HWPROBE_SENSOR_DYNAMIC = 1,
};

/* 值类型:决定 hwprobe_value / 推送 item 的 value 里哪个成员有效 */
enum {
    HWPROBE_VTYPE_F64  = 0,   /* double  —— value.f64   (温度/占用率/频率/电压…) */
    HWPROBE_VTYPE_U64  = 1,   /* uint64_t —— value.u64   (容量/计数/速率…) */
    HWPROBE_VTYPE_I64  = 2,   /* int64_t  —— value.i64   */
    HWPROBE_VTYPE_BOOL = 3,   /* uint8_t  —— value.b     (0/1,如链路是否连通) */
    HWPROBE_VTYPE_STR  = 4,   /* char[96] —— value.s     (UTF-8,如电池化学类型) */
};

/* ---------------------------------------------------------------------------
 * 结构体 —— 枚举静态信息用
 * ------------------------------------------------------------------------- */

/* 类别信息(hwprobe_get_category_info 的出参) */
typedef struct {
    uint32_t size;          /* = sizeof(hwprobe_category_info),DLL 回填 */
    uint32_t index;         /* 类别索引(HWPROBE_CAT_*) */
    char     name[32];      /* 类别名,UTF-8,如 "CPU" */
    uint32_t device_count;  /* 该类别下的设备数(类别不可用时为 0) */
} hwprobe_category_info;

/* 设备识别信息(hwprobe_get_device_info 的出参)。
 * 静态信息只在初始化时采集一次,不会变化。 */
typedef struct {
    uint32_t size;          /* = sizeof(hwprobe_device_info) */
    uint32_t category;      /* 所属类别索引 */
    uint32_t index;         /* 类别内设备下标 */
    char     id[128];       /* 跨启动稳定的实例 ID(PNP 路径 / PCI 地址 / GPU UUID 等) */
    char     name[192];     /* 设备名,如 "AMD Radeon RX 6800 XT" */
    char     vendor[96];    /* 厂商,可能为空串 */
    char     model[96];     /* 型号,可能为空串 */
    char     serial[96];    /* 序列号,可能为空串 */
} hwprobe_device_info;

/* 传感器清单项(hwprobe_get_sensor_info 的出参)。
 * code 是 provider 内部稳定键;name 是给用户看的展示名。 */
typedef struct {
    uint32_t size;          /* = sizeof(hwprobe_sensor_info) */
    char     code[64];      /* 稳定键,如 "load_core_0",可用于跨版本对齐 */
    char     name[96];      /* 展示名,如 "Core #0 Usage" */
    char     unit[16];      /* 单位,UTF-8:"%" "°C" "MB" "RPM"…;无量纲为空串 */
    uint32_t kind;          /* HWPROBE_SENSOR_*(静态/动态) */
    uint32_t value_type;    /* HWPROBE_VTYPE_*(决定 value 里哪个成员有效) */
} hwprobe_sensor_info;

/* 96 字节重叠 union:五种类型共享同一起始偏移,
 * 只有 value_type 对应的成员有效,读错成员得到的是无意义字节。
 * C# 等语言映射务必用显式重叠布局或 fixed buffer(见文件头第七节)。 */
typedef union {
    double   f64;           /* HWPROBE_VTYPE_F64 */
    int64_t  i64;           /* HWPROBE_VTYPE_I64 */
    uint64_t u64;           /* HWPROBE_VTYPE_U64 */
    uint8_t  b;             /* HWPROBE_VTYPE_BOOL */
    char     s[96];         /* HWPROBE_VTYPE_STR,UTF-8,NUL 结尾 */
} hwprobe_value;

/* 单传感器读值(hwprobe_read_sensor 的出参)。
 * 每个读值独立携带状态:调用返回 HWPROBE_OK 只代表“取到了这条记录”,
 * 记录本身是否可信要看 status 字段。 */
typedef struct {
    uint32_t size;          /* = sizeof(hwprobe_reading) */
    uint32_t value_type;    /* HWPROBE_VTYPE_* */
    uint32_t status;        /* HWPROBE_*(单传感器粒度,如 NO_DRIVER) */
    uint64_t timestamp_ms;  /* 采样时刻,Unix 毫秒 */
    hwprobe_value value;    /* 仅 value_type 对应的成员有效 */
} hwprobe_reading;

/* ---------------------------------------------------------------------------
 * 结构体 —— 订阅式流式推送(ABI >= 2)
 * ------------------------------------------------------------------------- */

/* 推送的单条样本:某个 (category, device, sensor) 动态传感器的一次读值。
 * 三元下标与枚举接口(hwprobe_get_*_info)完全一致,
 * 用枚举阶段建好的映射表即可还原名称。 */
typedef struct {
    uint32_t size;          /* = sizeof(hwprobe_stream_item) */
    uint32_t category;      /* 类别索引 HWPROBE_CAT_* */
    uint32_t device;        /* 设备下标 */
    uint32_t sensor;        /* 传感器下标 */
    uint32_t value_type;    /* HWPROBE_VTYPE_* */
    uint32_t status;        /* HWPROBE_*(单传感器粒度) */
    uint64_t timestamp_ms;  /* 该读值的采样时刻,Unix 毫秒 */
    hwprobe_value value;    /* 仅 value_type 对应的成员有效 */
} hwprobe_stream_item;

/* 推送的一批样本:一轮刷新后的完整动态传感器快照。
 * 注意:静态传感器不出现在推送里(它们不会变化),
 * 静态识别信息请用枚举接口取一次即可。 */
typedef struct {
    uint32_t size;          /* = sizeof(hwprobe_stream_batch) */
    uint32_t count;         /* items 数量 */
    uint64_t seq;           /* 批次号,单调递增,从 1 开始;可用于丢弃乱序/重复批 */
    uint64_t timestamp_ms;  /* 批次构建时刻,Unix 毫秒 */
    const hwprobe_stream_item *items;  /* 项数组;【仅本次回调执行期间有效】 */
} hwprobe_stream_batch;

/* 订阅回调原型。
 *
 * 执行环境与契约(违反可能死锁或崩溃):
 *  - 在 DLL 内部的采集线程上被调用,【必须快速返回】——
 *    只做数据拷贝或投递到自己的队列,把渲染/IO 留给主线程;
 *  - 不要在回调里调用 hwprobe_refresh / hwprobe_shutdown 等会阻塞
 *    或改变库状态的接口;hwprobe_read_sensor(纯读缓存)是安全的;
 *  - items 指针仅在本次回调执行期间有效,返回即失效;
 *  - hwprobe_unsubscribe 允许在回调内部退订自身;
 *  - 回调内的异常行为由 DLL 兜底:panic 被捕获记录,不影响后续推送。
 *
 * 实际推送频率 = 订阅间隔与后台节拍(hwprobe_set_poll_interval)取大,
 * 且受各数据源内部 TTL 限制;数据未变化时推送相同值,seq 仍递增。
 * 回调对象在各语言里都要保活:C# 委托存字段、Python ctypes 存变量。 */
typedef void (*hwprobe_stream_cb)(const hwprobe_stream_batch *batch, void *user_data);

/* ---------------------------------------------------------------------------
 * 导出函数
 * ------------------------------------------------------------------------- */

/* 查询 DLL 编译时的 ABI 版本。>= 2 才有订阅推送接口。
 * 无需 hwprobe_init,任何时候可调。 */
uint32_t hwprobe_abi_version(void);

/* 初始化:枚举全部硬件(WMI/SMBIOS/DXGI 等)+ 启动后台采集线程。
 * - 必须在其他接口之前调用;
 * - 幂等:重复调用直接返回 OK;
 * - 耗时约 1~2 秒(内部在专职线程上做,含 COM 初始化);
 * - 绝不自动安装内核驱动,驱动安装见 hwprobe_setup_driver();
 * - 失败返回非 0,原因经 hwprobe_last_error 获取。 */
int32_t  hwprobe_init(void);

/* 关闭:停止后台采集线程,释放资源。幂等。
 * 进程退出前调用一次即可;调用后请不要再调用其他接口。 */
int32_t  hwprobe_shutdown(void);

/* 设置后台采集节拍(毫秒),范围 100~60000,超出自动截断,默认 500。
 * 同时影响:拉取模式下数据的新鲜度上限、订阅推送的派发节拍。
 * 注意实际刷新频率还受各数据源自身 TTL 限制(多数为 1 秒)。 */
int32_t  hwprobe_set_poll_interval(uint32_t ms);

/* 占用口径(hwprobe_set_usage_mode 的 mode 参数)。
 * 两种口径只影响占用类传感器(CPU 总占用/逐核占用、GPU 总占用)的取值来源,
 * 传感器清单与编码不变;温度/功耗/风扇/显存等不受影响。 */
enum {
    /* 0(默认):CPU = 时间差分(GetSystemTimes / NtQuerySystemInformation),
     * GPU = 厂商 API(NVML/ADLX/IGCL)。纯"忙碌墙钟时间占比",与频率无关,
     * 与 HWiNFO 等工具一致。 */
    HWPROBE_USAGE_STANDARD    = 0,
    /* 1:类 Windows 11 24H2+ 任务管理器默认视图 —— CPU = PDH
     * "% Processor Utility" ÷ "% Processor Performance"(归一化,频率无关
     * 的忙碌占比,封顶 100%;数学上与标准口径等价,仅采样窗口不同);
     * GPU = Windows GPU Engine 引擎计数器按引擎类型聚合取最忙值
     * (游戏时≈任务管理器默认显示的 3D 引擎占用)。 */
    HWPROBE_USAGE_TASKMANAGER = 1,
};

/* 设置占用口径。可在 hwprobe_init() 之前或运行中任意时刻调用,
 * 下一次后台刷新(≤1s)即生效,随时可来回切换。
 * 逐核频率传感器(freq_core_*,两种口径)优先来自 PDH "Actual Frequency"
 * 计数器 —— 实时反映睿频/降频(与任务管理器底部的"速度"同源);
 * PDH 不可用时回退 CallNtPowerInformation(恒为标称频率,不随负载变化)。
 * 分引擎传感器 util_eng_3d / util_eng_copy / util_eng_video_decode /
 * util_eng_video_encode / util_eng_compute(两种口径都提供)始终来自
 * Windows GPU Engine 计数器,不受此设置影响。
 * 注意:任务管理器口径依赖 PDH(性能计数器基础设施);个别机器上 PDH 被
 * 安全软件破坏,此时口径内占用传感器状态为 ERROR(不回退到另一口径,
 * 避免口径混用),其余传感器与标准口径完全不受影响。
 * 未知 mode 返回 HWPROBE_INVALID_ARG。 */
int32_t  hwprobe_set_usage_mode(uint32_t mode);

/* ---- 枚举静态信息(设备/传感器清单)---- */

/* 取类别总数,写入 *out(当前为 9)。 */
int32_t  hwprobe_get_category_count(uint32_t *out);

/* 取一个类别的信息(名称 + 设备数)。
 * category:HWPROBE_CAT_* 下标。失败返回 BAD_CATEGORY 等。 */
int32_t  hwprobe_get_category_info(uint32_t category, hwprobe_category_info *out);

/* 取某类别的设备数,写入 *out。设备数可能为 0(该类别不可用/无设备)。 */
int32_t  hwprobe_get_device_count(uint32_t category, uint32_t *out);

/* 取设备识别信息(名称/厂商/型号/序列号/稳定 ID)。
 * 典型用法:先 get_device_count,再循环 0..count 逐个取。 */
int32_t  hwprobe_get_device_info(uint32_t category, uint32_t device, hwprobe_device_info *out);

/* 取某设备的传感器数量,写入 *out。 */
int32_t  hwprobe_get_sensor_count(uint32_t category, uint32_t device, uint32_t *out);

/* 取传感器元信息(名称/单位/类型/值类型)。
 * 典型用法:建立 (category,device,sensor) → 展示名 的映射表,
 * 供拉取循环或推送回调使用(item 里只有下标)。 */
int32_t  hwprobe_get_sensor_info(uint32_t category, uint32_t device, uint32_t sensor,
                                 hwprobe_sensor_info *out);

/* ---- 读值 ---- */

/* 立即刷新一个类别(HWPROBE_ALL_CATEGORIES = 全部),绕过数据源 TTL。
 * 同步阻塞到刷新完成(毫秒级)。通常【不需要】调用 —— 后台线程自动
 * 轮询;高频调用会明显增加 CPU 负载。仅在“强制立刻取一次最新值”时用。 */
int32_t  hwprobe_refresh(uint32_t category);

/* 读一个传感器的当前值(内部缓存,毫秒级返回,不触发采集)。
 *
 * 参数:三元下标 (category, device, sensor),来自枚举阶段。
 * 出参:hwprobe_reading,含独立状态码与时间戳。
 * 返回值:接口级结果(OK / NOT_INITIALIZED / INVALID_ARG / BAD_* …);
 *         读值本身是否可信再看 rd.status(如 WAITING / NO_DRIVER)。
 * 提示:刚 init 完动态值可能处于 WAITING。绝对量(温度/容量/占用率之外
 *       的即时值)一个采样点就有;吞吐/速率/占用这类**差分**读数要两个
 *       采样点,后台节拍 500ms 下通常 1 秒内就绪。读到 WAITING 时按
 *       "暂无值"处理,稍后重读即可,不要当成 0。 */
int32_t  hwprobe_read_sensor(uint32_t category, uint32_t device, uint32_t sensor,
                             hwprobe_reading *out);

/* 整棵树 JSON 便捷导出(UTF-8)。
 * 两次调用模式:第一次 buf 传 NULL,*len 返回所需字节数(含 NUL);
 * 分配好缓冲后再调一次完成拷贝。缓冲不足返回 BUFFER_TOO_SMALL 并
 * 更新 *len。适合调试/日志,不建议在渲染循环里高频调用。 */
int32_t  hwprobe_dump_json(uint32_t category, char *buf, uint32_t *len);

/* 取最近一条内部错误/诊断信息(UTF-8),同样支持两段式调用。
 * 无错误时返回 "no error"。更完整的诊断日志可用 hwprobe-cli log 查看。 */
int32_t  hwprobe_last_error(char *buf, uint32_t *len);

/* ---- 订阅式流式推送(ABI >= 2,推荐)---- */

/* 注册订阅:之后 DLL 在内部线程上按节拍把全部动态传感器快照推给回调,
 * 调用方无需任何轮询。
 *
 * 参数:
 *   callback      回调原型 hwprobe_stream_cb,不可为 NULL;
 *   user_data     原样回传给回调的上下文指针,可为 NULL;
 *   interval_ms   期望推送间隔:0 = 跟随后台节拍每次都推;
 *                 非零下限 100ms,并会自动把后台节拍调低到不超过它;
 *                 实际频率受数据源 TTL 限制,值未变时推相同值、seq 递增;
 *   category_mask 类别过滤:bit i = 类别 i(1u << HWPROBE_CAT_xxx),
 *                 HWPROBE_ALL_CATEGORIES = 全部,0 非法(返回 INVALID_ARG);
 *   out_handle    成功时写入订阅句柄,用于 hwprobe_unsubscribe。
 *
 * 返回:OK / NOT_INITIALIZED(未先 init)/ INVALID_ARG。
 * 订阅期间回调对象必须保活(见 hwprobe_stream_cb 注释)。 */
int32_t  hwprobe_subscribe(hwprobe_stream_cb callback, void *user_data,
                           uint32_t interval_ms, uint32_t category_mask,
                           uint64_t *out_handle);

/* 取消订阅。保证返回之后该订阅的回调不会再被调用(在途回调已结束)。
 * 可在回调内部退订自身;重复取消同一句柄返回 HWPROBE_BAD_HANDLE。
 * 不退订直接 shutdown 也可以(shutdown 会停掉全部推送)。 */
int32_t  hwprobe_unsubscribe(uint64_t handle);

/* ---- 内核驱动增强层 ---- */

/* 一键安装 PawnIO 驱动(CPU 核心温度/功耗、主板风扇/电压必需):
 * 运行 DLL 内置的官方签名安装器,弹 UAC 后 -install -silent 静默安装。
 * - 幂等:已安装直接返回 OK;
 * - 此为显式操作,hwprobe_init() 不会自动触发;
 * - 安装成功后需【重启调用进程】才会加载驱动,且访问 PawnIO 设备
 *   始终需要管理员权限;
 * - 失败原因经 hwprobe_last_error 获取;
 * - 环境变量 HWPROBE_NO_DRIVER=1 可让本进程完全跳过驱动加载。 */
int32_t  hwprobe_setup_driver(void);

#ifdef __cplusplus
}
#endif
#endif /* HWPROBE_H */
