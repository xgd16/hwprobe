// hwprobe 的 Go 调用示例:纯 Go(免 cgo),用 purego 在运行时动态加载 hwprobe
// 动态库并调用其 C ABI —— 思路与 py-demo(ctypes)、cs-demo(P/Invoke)一致。
//
// 用法(先构建动态库:cargo build --release):
//
//	go run .              # 打印设备树 + 订阅推送演示 10 秒
//	go run . --all        # 树里包含静态信息传感器(型号/序列号等)
//	go run . --stream     # 只看订阅推送批次(不打印树)
//	go run . --pull       # 拉取模式:每秒重读一遍,只打印变化值
//	go run . --json       # 每秒打印整棵树 JSON
//	go run . --seconds 5  # 运行 5 秒后退出(0 = 直到 Ctrl+C)
//
// 动态库查找顺序:HWPROBE_LIB 环境变量 → 可执行文件目录 → 当前目录 →
// ../../target/release → ../../target/debug → ../../bin/dll → 系统搜索路径。
// 跨平台库名:Windows=hwprobe.dll / macOS=libhwprobe.dylib / Linux=libhwprobe.so。
//
// 三个 ABI 要点(完整契约见 include/hwprobe.h):
//
//  1. hwprobe_value 是 96 字节 union,必须按 value_type 解码(见 value96);
//  2. 每个传感器独立携带 status —— 宁可缺数据也不给假数据,读到
//     "等待首刷 / 无驱动 / 不支持"时按"暂无值"处理,不要当成 0;
//  3. 订阅回调运行在 DLL 内部采集线程上,只做拷贝入队立即返回;
//     回调指针(purego.NewCallback 的返回值)必须保活到退订之后。
package main

import (
	"bytes"
	"encoding/binary"
	"flag"
	"fmt"
	"math"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"strings"
	"time"
	"unsafe"

	"github.com/ebitengine/purego"
)

// ---------- 常量(与 include/hwprobe.h 严格对应) ----------

const (
	allCategories = 0xFFFFFFFF // HWPROBE_ALL_CATEGORIES

	sensorStatic  = 0 // HWPROBE_SENSOR_STATIC
	sensorDynamic = 1 // HWPROBE_SENSOR_DYNAMIC

	vtypeF64  = 0 // HWPROBE_VTYPE_F64
	vtypeU64  = 1
	vtypeI64  = 2
	vtypeBool = 3
	vtypeStr  = 4

	statusOK = 0 // HWPROBE_OK

	refreshMs = 1000 // 刷新/推送周期(与 DLL 后台节拍对齐)
)

var statusName = map[uint32]string{
	0: "OK", 1: "等待首刷", 2: "未初始化", 3: "初始化失败",
	4: "参数无效", 5: "类别无效", 6: "设备无效", 7: "传感器无效",
	8: "无驱动", 9: "不支持", 10: "错误", 11: "缓冲不足", 12: "未知",
	13: "句柄无效", 14: "无权限",
}

func statusText(s uint32) string {
	if t, ok := statusName[s]; ok {
		return t
	}
	return fmt.Sprintf("码%d", s)
}

// ---------- 结构体(与 C 侧逐字节对齐;首字段 size 必须预填 sizeof) ----------

// value96 对应 hwprobe_value:96 字节 union,各类型共享偏移 0,
// 只有 value_type 对应的成员有效 —— 读错成员得到的是无意义字节。
// 这里用 [96]byte 接收再按类型手工解码(C# 示例同款坑,不能映射成一串字段)。
// 注:x64/ARM64 目标均为小端,直接按小端解码即可。
type value96 [96]byte

func (v value96) f64() float64 {
	return math.Float64frombits(binary.LittleEndian.Uint64(v[:8]))
}
func (v value96) u64() uint64 { return binary.LittleEndian.Uint64(v[:8]) }
func (v value96) i64() int64  { return int64(binary.LittleEndian.Uint64(v[:8])) }
func (v value96) b() bool     { return v[0] != 0 }
func (v value96) s() string   { return cstr(v[:]) }

// hwprobe_category_info
type categoryInfo struct {
	Size        uint32
	Index       uint32
	Name        [32]byte
	DeviceCount uint32
}

// hwprobe_device_info
type deviceInfo struct {
	Size     uint32
	Category uint32
	Index    uint32
	ID       [128]byte
	Name     [192]byte
	Vendor   [96]byte
	Model    [96]byte
	Serial   [96]byte
}

// hwprobe_sensor_info
type sensorInfo struct {
	Size      uint32
	Code      [64]byte
	Name      [96]byte
	Unit      [16]byte
	Kind      uint32
	ValueType uint32
}

// hwprobe_reading
type reading struct {
	Size        uint32
	ValueType   uint32
	Status      uint32
	TimestampMs uint64
	Value       value96
}

// hwprobe_stream_item
type streamItem struct {
	Size        uint32
	Category    uint32
	Device      uint32
	Sensor      uint32
	ValueType   uint32
	Status      uint32
	TimestampMs uint64
	Value       value96
}

// hwprobe_stream_batch
type streamBatch struct {
	Size        uint32
	Count       uint32
	Seq         uint64
	TimestampMs uint64
	Items       *streamItem
}

// 结构体布局错位是 FFI 最常见的坑:启动时先和 C ABI 的 sizeof 对一遍账。
func init() {
	checks := []struct {
		name string
		got  uintptr
		want uintptr
	}{
		{"hwprobe_category_info", unsafe.Sizeof(categoryInfo{}), 44},
		{"hwprobe_device_info", unsafe.Sizeof(deviceInfo{}), 620},
		{"hwprobe_sensor_info", unsafe.Sizeof(sensorInfo{}), 188},
		{"hwprobe_reading", unsafe.Sizeof(reading{}), 120},
		{"hwprobe_stream_item", unsafe.Sizeof(streamItem{}), 128},
		{"hwprobe_stream_batch", unsafe.Sizeof(streamBatch{}), 32},
	}
	for _, c := range checks {
		if c.got != c.want {
			panic(fmt.Sprintf("%s: Go 侧 sizeof=%d,与 C ABI 的 %d 不符,结构体布局需要修正",
				c.name, c.got, c.want))
		}
	}
}

// ---------- 小工具 ----------

// cstr:C 字符串(UTF-8,NUL 结尾)→ Go string
func cstr(b []byte) string {
	if i := bytes.IndexByte(b, 0); i >= 0 {
		b = b[:i]
	}
	return string(b)
}

// fmtValue 按 value_type 格式化 union 里的值(读值与推送共用)。
func fmtValue(vtype uint32, v value96) string {
	switch vtype {
	case vtypeF64:
		return fmt.Sprintf("%.1f", v.f64())
	case vtypeU64:
		return comma(int64(v.u64()))
	case vtypeI64:
		return comma(v.i64())
	case vtypeBool:
		if v.b() {
			return "是"
		}
		return "否"
	case vtypeStr:
		return v.s()
	}
	return "?"
}

// comma 千分位格式化(如 16384 → 16,384)
func comma(n int64) string {
	s := fmt.Sprintf("%d", n)
	neg := strings.HasPrefix(s, "-")
	if neg {
		s = s[1:]
	}
	var out []byte
	for i, c := range []byte(s) {
		if i > 0 && (len(s)-i)%3 == 0 {
			out = append(out, ',')
		}
		out = append(out, c)
	}
	if neg {
		return "-" + string(out)
	}
	return string(out)
}

// ---------- 动态库加载与函数绑定 ----------

// api:hwprobe 导出函数经 purego 绑定后的 Go 函数表。
// 参数/返回值与 include/hwprobe.h 里的原型逐个对应。
type api struct {
	abiVersion       func() uint32
	init             func() int32
	shutdown         func() int32
	setPollInterval  func(ms uint32) int32
	getCategoryCount func(out *uint32) int32
	getCategoryInfo  func(cat uint32, out *categoryInfo) int32
	getDeviceCount   func(cat uint32, out *uint32) int32
	getDeviceInfo    func(cat, dev uint32, out *deviceInfo) int32
	getSensorCount   func(cat, dev uint32, out *uint32) int32
	getSensorInfo    func(cat, dev, sen uint32, out *sensorInfo) int32
	readSensor       func(cat, dev, sen uint32, out *reading) int32
	dumpJSONFn       func(cat uint32, buf *byte, n *uint32) int32
	lastErrorFn      func(buf *byte, n *uint32) int32
	subscribe        func(cb, userData uintptr, intervalMs, mask uint32, out *uint64) int32 // ABI >= 2
	unsubscribe      func(h uint64) int32                                                   // ABI >= 2
}

// findLibrary 按查找顺序定位动态库(见文件头)。
func findLibrary() string {
	var names []string
	switch runtime.GOOS {
	case "windows":
		names = []string{"hwprobe.dll"}
	case "darwin":
		names = []string{"libhwprobe.dylib"}
	default:
		names = []string{"libhwprobe.so"}
	}
	if p := os.Getenv("HWPROBE_LIB"); p != "" {
		return p
	}
	var dirs []string
	if exe, err := os.Executable(); err == nil {
		dirs = append(dirs, filepath.Dir(exe))
	}
	if cwd, err := os.Getwd(); err == nil {
		// go run 的可执行文件在临时目录,所以还要试仓库里的构建产物目录
		dirs = append(dirs,
			cwd,
			filepath.Join(cwd, "..", "..", "target", "release"),
			filepath.Join(cwd, "..", "..", "target", "debug"),
			filepath.Join(cwd, "..", "..", "bin", "dll"),
		)
	}
	for _, d := range dirs {
		for _, n := range names {
			p := filepath.Join(d, n)
			if st, err := os.Stat(p); err == nil && !st.IsDir() {
				return p
			}
		}
	}
	return names[0] // 交给系统搜索路径
}

// bind 加载动态库并绑定全部导出函数。订阅接口是 ABI 2 才有的可选项。
func bind(path string) (*api, error) {
	h, err := loadLibrary(path)
	if err != nil {
		return nil, fmt.Errorf("加载 %s 失败:%w", path, err)
	}
	a := &api{}
	purego.RegisterLibFunc(&a.abiVersion, h, "hwprobe_abi_version")
	purego.RegisterLibFunc(&a.init, h, "hwprobe_init")
	purego.RegisterLibFunc(&a.shutdown, h, "hwprobe_shutdown")
	purego.RegisterLibFunc(&a.setPollInterval, h, "hwprobe_set_poll_interval")
	purego.RegisterLibFunc(&a.getCategoryCount, h, "hwprobe_get_category_count")
	purego.RegisterLibFunc(&a.getCategoryInfo, h, "hwprobe_get_category_info")
	purego.RegisterLibFunc(&a.getDeviceCount, h, "hwprobe_get_device_count")
	purego.RegisterLibFunc(&a.getDeviceInfo, h, "hwprobe_get_device_info")
	purego.RegisterLibFunc(&a.getSensorCount, h, "hwprobe_get_sensor_count")
	purego.RegisterLibFunc(&a.getSensorInfo, h, "hwprobe_get_sensor_info")
	purego.RegisterLibFunc(&a.readSensor, h, "hwprobe_read_sensor")
	purego.RegisterLibFunc(&a.dumpJSONFn, h, "hwprobe_dump_json")
	purego.RegisterLibFunc(&a.lastErrorFn, h, "hwprobe_last_error")
	if _, err := findSymbol(h, "hwprobe_subscribe"); err == nil {
		purego.RegisterLibFunc(&a.subscribe, h, "hwprobe_subscribe")
		purego.RegisterLibFunc(&a.unsubscribe, h, "hwprobe_unsubscribe")
	}
	return a, nil
}

// lastError 两段式取错误信息:第一次问长度,第二次取内容。
func (a *api) lastError() string {
	var n uint32
	a.lastErrorFn(nil, &n)
	if n == 0 {
		return ""
	}
	buf := make([]byte, n)
	if a.lastErrorFn(&buf[0], &n) != statusOK {
		return ""
	}
	return cstr(buf)
}

// dumpJSON 两段式导出整棵树 JSON(UTF-8)。
func (a *api) dumpJSON() string {
	var n uint32
	a.dumpJSONFn(allCategories, nil, &n)
	if n == 0 {
		return ""
	}
	buf := make([]byte, n)
	if a.dumpJSONFn(allCategories, &buf[0], &n) != statusOK {
		return ""
	}
	return cstr(buf)
}

// ---------- 枚举:类别 → 设备 → 传感器 ----------

// key 三元下标 (category, device, sensor),枚举与推送共用同一套下标。
type key struct{ cat, dev, sen uint32 }

type row struct {
	key       key
	catName   string
	devName   string
	devIdent  string // 厂商/型号/序列号(--all 时展示)
	name      string // 传感器展示名
	unit      string
	kind      uint32
	valueType uint32
}

// buildIndex 枚举整棵树。DLL 内部有 TTL 缓存,毫秒级返回。
// 返回扁平行(枚举顺序)+ (cat,dev,sen) → 行 的映射,供推送下标还原名称。
func (a *api) buildIndex(includeStatic bool) ([]row, map[key]row) {
	rows := []row{}
	names := map[key]row{}
	var nCat uint32
	if a.getCategoryCount(&nCat) != statusOK {
		return rows, names
	}
	for cat := uint32(0); cat < nCat; cat++ {
		var ci categoryInfo
		ci.Size = uint32(unsafe.Sizeof(ci))
		if a.getCategoryInfo(cat, &ci) != statusOK {
			continue
		}
		var nDev uint32
		if a.getDeviceCount(cat, &nDev) != statusOK {
			continue
		}
		for dev := uint32(0); dev < nDev; dev++ {
			var di deviceInfo
			di.Size = uint32(unsafe.Sizeof(di))
			if a.getDeviceInfo(cat, dev, &di) != statusOK {
				continue
			}
			ident := ""
			for _, part := range []string{cstr(di.Vendor[:]), cstr(di.Model[:]), cstr(di.Serial[:])} {
				if part != "" {
					if ident != "" {
						ident += " / "
					}
					ident += part
				}
			}
			var nSen uint32
			if a.getSensorCount(cat, dev, &nSen) != statusOK {
				continue
			}
			for sen := uint32(0); sen < nSen; sen++ {
				var si sensorInfo
				si.Size = uint32(unsafe.Sizeof(si))
				if a.getSensorInfo(cat, dev, sen, &si) != statusOK {
					continue
				}
				if si.Kind == sensorStatic && !includeStatic {
					continue
				}
				r := row{
					key:       key{cat, dev, sen},
					catName:   cstr(ci.Name[:]),
					devName:   cstr(di.Name[:]),
					devIdent:  ident,
					name:      cstr(si.Name[:]),
					unit:      cstr(si.Unit[:]),
					kind:      si.Kind,
					valueType: si.ValueType,
				}
				rows = append(rows, r)
				names[r.key] = r
			}
		}
	}
	return rows, names
}

// readCell 读一个传感器,返回 (显示文本, 状态码);状态码 0 = OK。
// 接口返回 OK 只代表"取到了这条记录",记录是否可信还要看 rd.status。
func (a *api) readCell(k key) (string, uint32) {
	var r reading
	r.Size = uint32(unsafe.Sizeof(r))
	rc := a.readSensor(k.cat, k.dev, k.sen, &r)
	if rc != statusOK {
		return statusText(uint32(rc)), uint32(rc)
	}
	if r.Status != statusOK {
		return statusText(r.Status), r.Status
	}
	return fmtValue(r.ValueType, r.Value), statusOK
}

// printTree 按 类别 → 设备 → 传感器 三层缩进打印,并附当前读值。
func printTree(rows []row, a *api, includeStatic bool) {
	var lastCat, lastDev = ^uint32(0), ^uint32(0)
	for _, r := range rows {
		if r.key.cat != lastCat {
			fmt.Println(r.catName)
			lastCat, lastDev = r.key.cat, ^uint32(0)
		}
		if r.key.dev != lastDev {
			fmt.Printf(" └ %s\n", r.devName)
			if includeStatic && r.devIdent != "" {
				fmt.Printf("     识别:%s\n", r.devIdent)
			}
			lastDev = r.key.dev
		}
		text, st := a.readCell(r.key)
		if st == statusOK && r.unit != "" {
			text += " " + r.unit
		}
		mark := ""
		if r.kind == sensorStatic {
			mark = "  [静态]"
		}
		if st != statusOK {
			mark += "  [" + statusText(st) + "]"
		}
		fmt.Printf("     ├ %-32s = %-20s%s\n", r.name, text, mark)
	}
}

// ---------- 取数方式 1:拉取(read_sensor 读 TTL 缓存) ----------

// runPull 每秒重读全部动态传感器,只打印发生变化的值。
// 读的是 DLL 内部缓存,不触发实际采集,放心循环。
func runPull(a *api, rows []row, seconds float64, stop chan os.Signal) {
	fmt.Println("拉取模式:每秒 hwprobe_read_sensor 重读,只打印变化值")
	prev := map[key]string{}
	first := true
	tick := time.NewTicker(time.Second)
	defer tick.Stop()
	deadline := after(seconds)
	for {
		select {
		case <-tick.C:
			for _, r := range rows {
				if r.kind != sensorDynamic {
					continue
				}
				text, _ := a.readCell(r.key)
				if !first && prev[r.key] == text {
					continue
				}
				prev[r.key] = text
				fmt.Printf("%s  %s/%s = %s %s\n",
					time.Now().Format("15:04:05"), r.catName, r.name, text, r.unit)
			}
			first = false
		case <-deadline:
			return
		case <-stop:
			return
		}
	}
}

// ---------- 取数方式 2:订阅推送(subscribe,推荐) ----------

// 推送批次的拷贝:回调里只搬数据,格式化留给主 goroutine。
type itemCopy struct {
	key       key
	valueType uint32
	status    uint32
	value     value96
}

type batchCopy struct {
	seq   uint64
	ts    uint64
	items []itemCopy
}

// streamCh 回调 → 主 goroutine 的队列;满了就丢批(seq 单调递增,可看出缺口)。
var streamCh = make(chan batchCopy, 16)

// streamCB 订阅回调函数指针。存包级变量保活到退订之后 ——
// 若被 GC 回收,DLL 再调用它就会崩(C# 委托存字段、Python ctypes 存变量同理)。
var streamCB = purego.NewCallback(onStreamBatch)

// onStreamBatch 订阅回调:运行在 DLL 内部采集线程上!
// 契约:只做拷贝入队,立即返回;不要在这里做 IO、阻塞或调用
// hwprobe_refresh / hwprobe_shutdown(items 指针仅本次回调有效)。
// 第一个参数直接声明成 *streamBatch(purego 会按 C 指针还原),
// 免去 uintptr → unsafe.Pointer 转换;返回值恒为 0:
// C 侧是 void 回调,但 Windows 回调约定要求带一个返回值。
func onStreamBatch(batch *streamBatch, userData uintptr) uintptr {
	b := batch
	if b == nil || b.Items == nil || b.Count == 0 {
		return 0
	}
	c := batchCopy{seq: b.Seq, ts: b.TimestampMs, items: make([]itemCopy, 0, b.Count)}
	src := unsafe.Slice(b.Items, int(b.Count))
	for i := range src {
		it := &src[i]
		c.items = append(c.items, itemCopy{
			key:       key{it.Category, it.Device, it.Sensor},
			valueType: it.ValueType,
			status:    it.Status,
			value:     it.Value,
		})
	}
	select {
	case streamCh <- c:
	default: // 主线程跟不上就丢掉本批,绝不阻塞采集线程
	}
	return 0
}

// runStream 订阅推送演示:数据由 DLL 按节拍主动推来,本进程不做任何轮询。
func runStream(a *api, names map[key]row, seconds float64, stop chan os.Signal) {
	if a.subscribe == nil {
		fmt.Fprintln(os.Stderr, "当前动态库无订阅接口(ABI < 2),请重新构建;退回拉取模式")
		return
	}
	var h uint64
	// streamCB 作为参数传入后,回调指针的存活由包级变量 streamCB 兜底
	rc := a.subscribe(streamCB, 0, refreshMs, allCategories, &h)
	if rc != statusOK {
		fmt.Fprintf(os.Stderr, "hwprobe_subscribe 失败:%s  %s\n", statusText(uint32(rc)), a.lastError())
		return
	}
	fmt.Printf("订阅推送模式:每 %dms 一批,由 DLL 内部线程推送,Ctrl+C 退出\n", refreshMs)
	deadline := after(seconds)
	for {
		select {
		case b := <-streamCh:
			printBatch(b, names)
		case <-deadline:
			a.unsubscribe(h)
			fmt.Println("已退订")
			return
		case <-stop:
			a.unsubscribe(h)
			fmt.Println("已退订")
			return
		}
	}
}

// printBatch 打印一批推送:批号、时间、项数与前几个正常读值。
func printBatch(b batchCopy, names map[key]row) {
	okN := 0
	var samples []string
	for _, it := range b.items {
		if it.status != statusOK {
			continue
		}
		okN++
		if len(samples) < 6 {
			r := names[it.key]
			samples = append(samples, fmt.Sprintf("%s/%s=%s %s",
				r.catName, r.name, fmtValue(it.valueType, it.value), r.unit))
		}
	}
	fmt.Printf("#%d %s —— %d 项(OK %d): %s\n",
		b.seq, time.UnixMilli(int64(b.ts)).Format("15:04:05.000"),
		len(b.items), okN, strings.Join(samples, " | "))
}

// ---------- dump_json 模式 ----------

func runJSON(a *api, seconds float64, stop chan os.Signal) {
	tick := time.NewTicker(time.Second)
	defer tick.Stop()
	deadline := after(seconds)
	for {
		select {
		case <-tick.C:
			if s := a.dumpJSON(); s != "" {
				fmt.Println(s)
			}
		case <-deadline:
			return
		case <-stop:
			return
		}
	}
}

// after:seconds > 0 时返回到期通道,0 = 永不到期(直到 Ctrl+C)。
func after(seconds float64) <-chan time.Time {
	if seconds <= 0 {
		return nil
	}
	return time.After(time.Duration(seconds * float64(time.Second)))
}

// ---------- 入口 ----------

func main() {
	all := flag.Bool("all", false, "设备树中包含静态信息传感器(型号/序列号等)")
	jsonMode := flag.Bool("json", false, "控制台 JSON 模式(每秒打印整棵树)")
	streamMode := flag.Bool("stream", false, "只看订阅推送批次(不打印树)")
	pullMode := flag.Bool("pull", false, "拉取模式:每秒重读,只打印变化值(不打印树)")
	seconds := flag.Float64("seconds", 10, "运行时长(秒),0 = 直到 Ctrl+C")
	flag.Parse()

	a, err := bind(findLibrary())
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	abi := a.abiVersion()
	fmt.Printf("hwprobe ABI %d\n", abi)
	if abi < 2 {
		fmt.Fprintln(os.Stderr, "警告:ABI < 2,无订阅推送接口;将退回拉取模式")
	}

	// hwprobe_init:枚举硬件 + 启动后台采集线程(1~2 秒,幂等,绝不自动装驱动)
	if rc := a.init(); rc != statusOK {
		fmt.Fprintf(os.Stderr, "hwprobe_init 失败:%s  %s\n", statusText(uint32(rc)), a.lastError())
		os.Exit(1)
	}
	defer a.shutdown()
	a.setPollInterval(refreshMs)

	// 占用率/吞吐这类差分量要两个采样点,等一轮再读,否则全是"等待首刷"
	time.Sleep(1200 * time.Millisecond)

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt)

	rows, names := a.buildIndex(*all)
	if !*jsonMode && !*streamMode && !*pullMode {
		printTree(rows, a, *all)
		fmt.Println()
	}

	switch {
	case *jsonMode:
		runJSON(a, *seconds, stop)
	case *pullMode:
		runPull(a, rows, *seconds, stop)
	case *streamMode:
		runStream(a, names, *seconds, stop)
	default:
		if a.subscribe != nil {
			runStream(a, names, *seconds, stop)
		} else {
			runPull(a, rows, *seconds, stop)
		}
	}
}
