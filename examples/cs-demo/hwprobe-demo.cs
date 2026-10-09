// C# P/Invoke 调用 hwprobe.dll 的最小示例。
// 说明:DLL 内字符串一律 UTF-8;结构体里用 byte 数组接收再手动解码,
// 避免 CharSet.Ansi 在中文系统(GBK)下产生乱码。
// 编译:csc /out:hwprobe-demo.exe hwprobe-demo.cs
// 运行:把 hwprobe.dll 与本程序放同一目录后运行 hwprobe-demo.exe
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class Hw
{
    public const uint AllCategories = 0xFFFFFFFF;

    [StructLayout(LayoutKind.Sequential)]
    public struct DeviceInfo
    {
        public uint size, category, index;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 128)] public byte[] id;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 192)] public byte[] name;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 96)]  public byte[] vendor;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 96)]  public byte[] model;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 96)]  public byte[] serial;

        public string Id => Utf8(id);
        public string Name => Utf8(name);
        public string Vendor => Utf8(vendor);
        public string Model => Utf8(model);
        public string Serial => Utf8(serial);

        internal static string Utf8(byte[] b)
        {
            int end = Array.IndexOf(b, (byte)0);
            return Encoding.UTF8.GetString(b, 0, end < 0 ? b.Length : end);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct SensorInfo
    {
        public uint size;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 64)] public byte[] code;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 96)] public byte[] name;
        [MarshalAs(UnmanagedType.ByValArray, SizeConst = 16)] public byte[] unit;
        public uint kind, valueType;

        public string Code => DeviceInfo.Utf8(code);
        public string Name => DeviceInfo.Utf8(name);
        public string Unit => DeviceInfo.Utf8(unit);
    }

    // 值区是 C 侧的 union:96 字节,各类型都从偏移 0 开始。
    // 用 fixed buffer + 手动解码,避免 Sequential 结构里字符串错位
    // (union 区必须用显式重叠布局,不能用一堆 Sequential 字段)。
    [StructLayout(LayoutKind.Sequential)]
    public unsafe struct Reading
    {
        public uint size, valueType, status;
        public ulong timestampMs;
        public fixed byte value[96];

        double U8ToF64(int off)
        {
            var b = new byte[8];
            for (int i = 0; i < 8; i++) b[i] = value[off + i];
            return BitConverter.ToDouble(b, 0);
        }
        ulong U8ToU64(int off)
        {
            var b = new byte[8];
            for (int i = 0; i < 8; i++) b[i] = value[off + i];
            return BitConverter.ToUInt64(b, 0);
        }

        public double F64 => U8ToF64(0);
        public long I64 => unchecked((long)U8ToU64(0));
        public ulong U64 => U8ToU64(0);
        public bool B => value[0] != 0;
        public string S
        {
            get
            {
                int n = 0;
                while (n < 96 && value[n] != 0) n++;
                var b = new byte[n];
                for (int i = 0; i < n; i++) b[i] = value[i];
                return Encoding.UTF8.GetString(b);
            }
        }
    }

    // ---- 订阅式流式推送(ABI >= 2) ----
    // items 指针仅在回调执行期间有效,回调里必须立刻拷贝需要的数据。
    [StructLayout(LayoutKind.Sequential)]
    public unsafe struct StreamItem
    {
        public uint size, category, device, sensor, valueType, status;
        public ulong timestampMs;
        public fixed byte value[96];

        double U8ToF64(int off)
        {
            var b = new byte[8];
            for (int i = 0; i < 8; i++) b[i] = value[off + i];
            return BitConverter.ToDouble(b, 0);
        }
        ulong U8ToU64(int off)
        {
            var b = new byte[8];
            for (int i = 0; i < 8; i++) b[i] = value[off + i];
            return BitConverter.ToUInt64(b, 0);
        }

        public double F64 => U8ToF64(0);
        public long I64 => unchecked((long)U8ToU64(0));
        public ulong U64 => U8ToU64(0);
        public bool B => value[0] != 0;
        public string S
        {
            get
            {
                int n = 0;
                while (n < 96 && value[n] != 0) n++;
                var b = new byte[n];
                for (int i = 0; i < n; i++) b[i] = value[i];
                return Encoding.UTF8.GetString(b);
            }
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct StreamBatch
    {
        public uint size, count;
        public ulong seq, timestampMs;
        public IntPtr items;   // StreamItem*,仅回调期间有效
    }

    // 回调在 DLL 内部线程上执行:只做拷贝/入队,立即返回。
    // 委托必须保持引用(存到字段),否则被 GC 后原生回调会崩。
    public delegate void StreamCallback(IntPtr batchPtr, IntPtr userData);

    [DllImport("hwprobe")] public static extern int hwprobe_subscribe(
        StreamCallback callback, IntPtr userData, uint intervalMs, uint categoryMask,
        out ulong handle);
    [DllImport("hwprobe")] public static extern int hwprobe_unsubscribe(ulong handle);

    [DllImport("hwprobe")] public static extern uint hwprobe_abi_version();
    [DllImport("hwprobe")] public static extern int hwprobe_init();
    [DllImport("hwprobe")] public static extern int hwprobe_shutdown();
    [DllImport("hwprobe")] public static extern int hwprobe_get_category_count(out uint n);
    [DllImport("hwprobe")] public static extern int hwprobe_get_device_count(uint cat, out uint n);
    [DllImport("hwprobe")] public static extern int hwprobe_get_device_info(uint cat, uint dev, out DeviceInfo info);
    [DllImport("hwprobe")] public static extern int hwprobe_get_sensor_count(uint cat, uint dev, out uint n);
    [DllImport("hwprobe")] public static extern int hwprobe_get_sensor_info(uint cat, uint dev, uint sen, out SensorInfo info);
    [DllImport("hwprobe")] public static extern int hwprobe_read_sensor(uint cat, uint dev, uint sen, out Reading rd);
}

public static class Demo
{
    static readonly string[] StatusText =
    {
        "OK", "等待首刷", "未初始化", "初始化失败", "参数错误",
        "类别无效", "设备无效", "传感器无效", "需要驱动", "不支持", "错误", "缓冲区不足", "未知",
        "句柄无效",
    };
    static readonly string[] CategoryText = { "CPU", "Memory", "GPU", "Disk", "Network", "Battery", "Motherboard", "Display", "IMU" };

    static Hw.StreamCallback _streamCb; // 字段引用防 GC,订阅期间必须存活
    static readonly AutoResetEvent BatchArrived = new(false);

    static string StreamValue(Hw.StreamItem it) => it.valueType switch
    {
        0 => it.F64.ToString("0.#"),
        1 => it.U64.ToString(),
        2 => it.I64.ToString(),
        3 => it.B.ToString(),
        4 => it.S,
        _ => "?",
    };

    static unsafe void OnBatch(IntPtr batchPtr, IntPtr userData)
    {
        // DLL 内部线程:只读指针 + 打印,立刻返回
        var b = *(Hw.StreamBatch*)batchPtr;
        var items = (Hw.StreamItem*)b.items;
        string first = "";
        for (uint i = 0; i < b.count; i++)
        {
            if (items[i].status == 0)
            {
                var it = items[i];
                string cat = it.category < 9 ? CategoryText[it.category] : "?";
                first = $"{cat}[{it.device}].{it.sensor} = {StreamValue(it)}";
                break;
            }
        }
        Console.WriteLine($"  推送批次#{b.seq}: {b.count} 项,首个 OK 项 {first}");
        BatchArrived.Set();
    }

    public static void Main()
    {
        Console.OutputEncoding = Encoding.UTF8;
        Console.WriteLine($"hwprobe abi = {Hw.hwprobe_abi_version()}");
        if (Hw.hwprobe_init() != 0)
        {
            Console.WriteLine("hwprobe_init 失败");
            return;
        }
        Thread.Sleep(2000); // 等后台首轮刷新

        for (uint cat = 0; cat < CategoryText.Length; cat++)
        {
            if (Hw.hwprobe_get_device_count(cat, out uint devCount) != 0) continue;
            Console.WriteLine($"== {CategoryText[cat]} ==");
            for (uint d = 0; d < devCount; d++)
            {
                if (Hw.hwprobe_get_device_info(cat, d, out var dev) != 0) continue;
                Console.WriteLine($"  [{d}] {dev.Name}  (vendor: {dev.Vendor}, serial: {dev.Serial})");
                Hw.hwprobe_get_sensor_count(cat, d, out uint senCount);
                for (uint s = 0; s < senCount; s++)
                {
                    if (Hw.hwprobe_get_sensor_info(cat, d, s, out var sen) != 0) continue;
                    if (Hw.hwprobe_read_sensor(cat, d, s, out var rd) != 0) continue;
                    string val = rd.valueType switch
                    {
                        0 => rd.F64.ToString("0.#"),
                        1 => rd.U64.ToString(),
                        2 => rd.I64.ToString(),
                        3 => rd.B.ToString(),
                        4 => rd.S,
                        _ => "?",
                    };
                    string status = rd.status < StatusText.Length ? StatusText[rd.status] : rd.status.ToString();
                    Console.WriteLine($"      {sen.Name,-42} = {val} {sen.Unit}  ({status})");
                }
            }
        }
        // ---- 订阅推送演示:注册回调后不再主动读值,等 DLL 推 3 批 ----
        Console.WriteLine("\n== 订阅推送(3 批,1000ms)==");
        _streamCb = OnBatch;
        if (Hw.hwprobe_subscribe(_streamCb, IntPtr.Zero, 1000, Hw.AllCategories, out ulong h) == 0)
        {
            for (int i = 0; i < 3; i++)
            {
                if (!BatchArrived.WaitOne(5000))
                {
                    Console.WriteLine("  超时未收到推送");
                    break;
                }
            }
            Hw.hwprobe_unsubscribe(h);
            Console.WriteLine("  已退订");
        }
        else
        {
            Console.WriteLine("  hwprobe_subscribe 失败(旧 DLL?)");
        }

        Hw.hwprobe_shutdown();
    }
}
