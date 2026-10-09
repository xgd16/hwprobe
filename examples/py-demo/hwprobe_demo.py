#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
hwprobe.dll — Python (ctypes + Tkinter) 硬件传感器仪表盘,订阅式推送刷新

用法:
    python hwprobe_demo.py             # Tk 窗口仪表盘(订阅推送驱动,无需轮询)
    python hwprobe_demo.py --all       # 启动时即显示静态信息传感器
    python hwprobe_demo.py --setup     # 先一键安装 PawnIO 驱动(弹 UAC),再开窗口
    python hwprobe_demo.py --json      # 控制台模式:每秒打印整棵树 JSON
    python hwprobe_demo.py --stream    # 控制台模式:订阅推送演示(不轮询)
    python hwprobe_demo.py --stream --seconds 5   # 同上,5 秒后自动退出

DLL 查找顺序:本脚本所在目录 → 仓库 target/release → 系统 PATH。
需先 cargo build --release 生成 target/release/hwprobe.dll。
CPU 温度/功耗、主板风扇/电压需要 PawnIO 驱动(--setup 一键安装)并以管理员
运行;未加载驱动时这些传感器不会列出,状态栏会给出提示。
内存温度还需要内存条本身带温度传感器(DDR4 为可选件,DDR5 基本标配),
没有传感器的内存条不会出现 Temperature 行;未勾选"显示静态信息"时,
纯静态信息的内存条设备节点本身也不显示(用 --all 可查看识别信息)。
"""

import argparse
import ctypes as C
import os
import queue
import sys
import time
from collections import namedtuple
from datetime import datetime
from pathlib import Path

import tkinter as tk
from tkinter import ttk, messagebox

# ---------- 常量(与 include/hwprobe.h 严格对应) ----------
HWPROBE_ALL_CATEGORIES = 0xFFFFFFFF

SENSOR_STATIC, SENSOR_DYNAMIC = 0, 1
VTYPE_F64, VTYPE_U64, VTYPE_I64, VTYPE_BOOL, VTYPE_STR = 0, 1, 2, 3, 4

REFRESH_MS = 1000  # 刷新周期(与 DLL 后台轮询一致)

STATUS_NAME = {
    0: "OK", 1: "等待首刷", 2: "未初始化", 3: "初始化失败",
    4: "参数无效", 5: "类别无效", 6: "设备无效", 7: "传感器无效",
    8: "无驱动", 9: "不支持", 10: "错误", 11: "缓冲不足", 12: "未知",
    13: "句柄无效",
}

# ---------- 结构体(契约:首字段 size 必须写 sizeof) ----------
class HwCategoryInfo(C.Structure):
    _fields_ = [("size", C.c_uint32), ("index", C.c_uint32),
                ("name", C.c_char * 32), ("device_count", C.c_uint32)]

class HwDeviceInfo(C.Structure):
    _fields_ = [("size", C.c_uint32), ("category", C.c_uint32), ("index", C.c_uint32),
                ("id", C.c_char * 128), ("name", C.c_char * 192),
                ("vendor", C.c_char * 96), ("model", C.c_char * 96),
                ("serial", C.c_char * 96)]

class HwSensorInfo(C.Structure):
    _fields_ = [("size", C.c_uint32), ("code", C.c_char * 64), ("name", C.c_char * 96),
                ("unit", C.c_char * 16), ("kind", C.c_uint32), ("value_type", C.c_uint32)]

class HwValue(C.Union):
    # 96 字节重叠 union:f64/i64/u64/bool/char[96] 共享同一起始偏移
    _fields_ = [("f64", C.c_double), ("i64", C.c_int64), ("u64", C.c_uint64),
                ("b", C.c_uint8), ("s", C.c_char * 96)]

class HwReading(C.Structure):
    _fields_ = [("size", C.c_uint32), ("value_type", C.c_uint32), ("status", C.c_uint32),
                ("timestamp_ms", C.c_uint64), ("value", HwValue)]

# ---- 订阅式流式推送(ABI >= 2) ----
class HwStreamItem(C.Structure):
    _fields_ = [("size", C.c_uint32), ("category", C.c_uint32), ("device", C.c_uint32),
                ("sensor", C.c_uint32), ("value_type", C.c_uint32), ("status", C.c_uint32),
                ("timestamp_ms", C.c_uint64), ("value", HwValue)]

class HwStreamBatch(C.Structure):
    _fields_ = [("size", C.c_uint32), ("count", C.c_uint32),
                ("seq", C.c_uint64), ("timestamp_ms", C.c_uint64),
                ("items", C.POINTER(HwStreamItem))]

STREAM_CB = C.CFUNCTYPE(None, C.POINTER(HwStreamBatch), C.c_void_p)

Row = namedtuple("Row", "cat cat_name dev_name sen_name unit kind key")

def cstr(b):
    return b.split(b"\0", 1)[0].decode("utf-8", "replace")

def load_dll():
    here = Path(__file__).resolve().parent
    candidates = [here / "hwprobe.dll",
                  here / ".." / ".." / "target" / "release" / "hwprobe.dll"]
    for p in candidates:
        if p.is_file():
            try:
                os.add_dll_directory(str(p.parent))
            except OSError:
                pass
            return C.WinDLL(str(p))
    return C.WinDLL("hwprobe.dll")  # 交给系统搜索路径

def bind(dll):
    dll.hwprobe_abi_version.restype = C.c_uint32
    dll.hwprobe_init.restype = C.c_int32
    dll.hwprobe_shutdown.restype = C.c_int32
    dll.hwprobe_set_poll_interval.argtypes = [C.c_uint32]
    dll.hwprobe_set_poll_interval.restype = C.c_int32
    dll.hwprobe_get_category_count.argtypes = [C.POINTER(C.c_uint32)]
    dll.hwprobe_get_category_count.restype = C.c_int32
    dll.hwprobe_get_device_count.argtypes = [C.c_uint32, C.POINTER(C.c_uint32)]
    dll.hwprobe_get_device_count.restype = C.c_int32
    dll.hwprobe_get_sensor_count.argtypes = [C.c_uint32, C.c_uint32, C.POINTER(C.c_uint32)]
    dll.hwprobe_get_sensor_count.restype = C.c_int32
    dll.hwprobe_get_category_info.argtypes = [C.c_uint32, C.POINTER(HwCategoryInfo)]
    dll.hwprobe_get_device_info.argtypes = [C.c_uint32, C.c_uint32, C.POINTER(HwDeviceInfo)]
    dll.hwprobe_get_sensor_info.argtypes = [C.c_uint32, C.c_uint32, C.c_uint32,
                                            C.POINTER(HwSensorInfo)]
    dll.hwprobe_read_sensor.argtypes = [C.c_uint32, C.c_uint32, C.c_uint32,
                                        C.POINTER(HwReading)]
    dll.hwprobe_read_sensor.restype = C.c_int32
    dll.hwprobe_dump_json.argtypes = [C.c_uint32, C.c_char_p, C.POINTER(C.c_uint32)]
    dll.hwprobe_dump_json.restype = C.c_int32
    dll.hwprobe_last_error.argtypes = [C.c_char_p, C.POINTER(C.c_uint32)]
    dll.hwprobe_last_error.restype = C.c_int32
    if hasattr(dll, "hwprobe_subscribe"):
        dll.hwprobe_subscribe.argtypes = [STREAM_CB, C.c_void_p, C.c_uint32,
                                          C.c_uint32, C.POINTER(C.c_uint64)]
        dll.hwprobe_subscribe.restype = C.c_int32
        dll.hwprobe_unsubscribe.argtypes = [C.c_uint64]
        dll.hwprobe_unsubscribe.restype = C.c_int32

def last_error(dll):
    n = C.c_uint32(0)
    dll.hwprobe_last_error(None, C.byref(n))
    if n.value == 0:
        return ""
    buf = C.create_string_buffer(n.value)
    if dll.hwprobe_last_error(buf, C.byref(n)) == 0:
        return cstr(buf.value)
    return ""

def enumerate_tree(dll, include_static):
    """枚举全部 类别→设备→传感器。DLL 内部有 TTL 缓存,毫秒级返回。"""
    n_cat = C.c_uint32(0)
    if dll.hwprobe_get_category_count(C.byref(n_cat)) != 0:
        return []
    rows = []
    for cat in range(n_cat.value):
        ci = HwCategoryInfo(); ci.size = C.sizeof(HwCategoryInfo)
        if dll.hwprobe_get_category_info(cat, C.byref(ci)) != 0:
            continue
        n_dev = C.c_uint32(0)
        if dll.hwprobe_get_device_count(cat, C.byref(n_dev)) != 0:
            continue
        for dev in range(n_dev.value):
            di = HwDeviceInfo(); di.size = C.sizeof(HwDeviceInfo)
            if dll.hwprobe_get_device_info(cat, dev, C.byref(di)) != 0:
                continue
            n_sen = C.c_uint32(0)
            if dll.hwprobe_get_sensor_count(cat, dev, C.byref(n_sen)) != 0:
                continue
            for sen in range(n_sen.value):
                si = HwSensorInfo(); si.size = C.sizeof(HwSensorInfo)
                if dll.hwprobe_get_sensor_info(cat, dev, sen, C.byref(si)) != 0:
                    continue
                if si.kind == SENSOR_STATIC and not include_static:
                    continue
                rows.append(Row(cat, cstr(ci.name), cstr(di.name), cstr(si.name),
                                cstr(si.unit), si.kind, (cat, dev, sen)))
    return rows

def fmt_value(vtype, value):
    """按 value_type 格式化 union 里的值(读值与推送共用)。"""
    if vtype == VTYPE_F64:
        return f"{value.f64:.1f}"
    if vtype == VTYPE_U64:
        return f"{value.u64:,}"
    if vtype == VTYPE_I64:
        return f"{value.i64:,}"
    if vtype == VTYPE_BOOL:
        return "是" if value.b else "否"
    if vtype == VTYPE_STR:
        return cstr(value.s)
    return "?"

def read_cell(dll, key):
    """读一个传感器,返回 (显示文本, 状态码);状态码 0 = OK"""
    r = HwReading(); r.size = C.sizeof(HwReading)
    rc = dll.hwprobe_read_sensor(*key, C.byref(r))
    if rc != 0:
        return STATUS_NAME.get(rc, f"码{rc}"), rc
    if r.status != 0:
        return STATUS_NAME.get(r.status, f"码{r.status}"), r.status
    return fmt_value(r.value_type, r.value), 0

def is_admin():
    try:
        return bool(C.windll.shell32.IsUserAnAdmin())
    except Exception:
        return False

def setup_driver(dll):
    """调用 hwprobe_setup_driver 一键安装 PawnIO(内置官方安装器,弹 UAC)"""
    fn = getattr(dll, "hwprobe_setup_driver", None)
    if fn is None:
        print("当前 hwprobe.dll 未导出 hwprobe_setup_driver(请重新构建)", file=sys.stderr)
        return False
    fn.restype = C.c_int32
    rc = fn()
    if rc == 0:
        print("PawnIO 驱动:已安装 ✓" + ("" if is_admin() else "(仍需管理员运行才能读取温度/风扇)"))
        return True
    print(f"PawnIO 驱动:安装失败[{STATUS_NAME.get(rc, rc)}] {last_error(dll)}", file=sys.stderr)
    return False

def driver_layer_active(rows):
    """驱动增强层是否加载:CPU 类别下有温度传感器(DTS/Tctl)即视为可用。
    非管理员或未装 PawnIO 时,驱动级传感器根本不会被枚举,而不是报错。"""
    return any(r.cat == 0 and r.kind == SENSOR_DYNAMIC
               and "temperature" in r.sen_name.lower() for r in rows)

def memory_temp_present(rows):
    """是否存在内存条温度传感器(温度是动态传感器,仅在内存条带 TS 且
    驱动/管理员可用时由 DLL 声明)。"""
    return any(r.cat == 1 and r.kind == SENSOR_DYNAMIC
               and "temperature" in r.sen_name.lower() for r in rows)

# ---------------- 订阅式流式推送 ----------------

def subscribe_stream(dll, interval_ms, on_batch, mask=HWPROBE_ALL_CATEGORIES):
    """注册订阅,把推送转成 on_batch(seq, timestamp_ms, items) 回调。
    items 是 [( (cat,dev,sen), 显示文本, 状态码 ), ...] 的快照拷贝,
    在 DLL 内部线程上被调用 —— 只做拷贝入队,立即返回。
    返回 (句柄, 回调对象) 或 None(ABI < 2 或订阅失败)。
    回调对象必须保持引用到退订之后,否则会被 GC。"""
    if not hasattr(dll, "hwprobe_subscribe"):
        return None

    def trampoline(bp, _ud):
        b = bp[0]
        items = []
        for i in range(b.count):
            it = b.items[i]
            items.append(((it.category, it.device, it.sensor),
                          fmt_value(it.value_type, it.value), it.status))
        on_batch(b.seq, b.timestamp_ms, items)

    cb = STREAM_CB(trampoline)
    h = C.c_uint64(0)
    rc = dll.hwprobe_subscribe(cb, None, interval_ms, mask, C.byref(h))
    if rc != 0:
        return None
    return h, cb

def unsubscribe_stream(dll, sub):
    if sub is not None:
        rc = dll.hwprobe_unsubscribe(sub[0])
        if rc != 0:
            print(f"unsubscribe 失败:{STATUS_NAME.get(rc, rc)}", file=sys.stderr)

def loop_stream(dll, seconds=None):
    """控制台订阅模式:数据由 DLL 推送,本进程不再轮询。"""
    import queue
    q = queue.Queue()
    sub = subscribe_stream(dll, REFRESH_MS, lambda seq, ts, items: q.put((seq, ts, items)))
    if sub is None:
        print("当前 hwprobe.dll 无订阅接口(ABI < 2),请重新构建", file=sys.stderr)
        return
    print(f"stream 订阅模式:每 {REFRESH_MS}ms 一批,由 DLL 推送,Ctrl+C 退出")
    deadline = (time.time() + seconds) if seconds else None
    try:
        while True:
            try:
                seq, ts, items = q.get(timeout=0.5)
            except queue.Empty:
                if deadline and time.time() >= deadline:
                    break
                continue
            ok_n = sum(1 for _, _, st in items if st == 0)
            samples = [f"{t}" for _, t, st in items if st == 0][:6]
            print(f"#{seq} {datetime.fromtimestamp(ts / 1000):%H:%M:%S}.{ts % 1000:03d}"
                  f" —— {len(items)} 项(OK {ok_n}): " + " | ".join(samples))
            if deadline and time.time() >= deadline:
                break
    except KeyboardInterrupt:
        pass
    finally:
        unsubscribe_stream(dll, sub)
        print("已退订")

# ---------------- Tk 仪表盘 ----------------

class Dashboard:
    def __init__(self, root, dll, all_rows, show_static, use_stream=True):
        self.root = root
        self.dll = dll
        self.all_rows = all_rows
        self.drv_ok = driver_layer_active(all_rows)
        self.mem_temp_ok = memory_temp_present(all_rows)
        self.leaf_rows = {}  # iid -> Row(当前树里的叶子)

        # 订阅推送:DLL 内部线程只往队列里塞快照,主循环 after() 消费,
        # 不再逐行轮询 DLL。旧 DLL(ABI<2)或订阅失败时退回轮询模式。
        self.stream = None
        if use_stream:
            q = queue.Queue()
            sub = subscribe_stream(dll, REFRESH_MS,
                                   lambda seq, ts, items: q.put((seq, items)))
            if sub is not None:
                self.stream = (sub, q)

        self.show_static = tk.BooleanVar(value=show_static)
        self.filter_var = tk.StringVar()

        self._build_ui()
        self._rebuild()
        self.root.after(REFRESH_MS, self._tick)

    def _build_ui(self):
        self.root.title("hwprobe 硬件传感器仪表盘")
        self.root.geometry("860x700")
        self.root.minsize(640, 420)
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)

        top = ttk.Frame(self.root, padding=(8, 8, 8, 4))
        top.pack(fill="x")
        ttk.Label(top, text="hwprobe 硬件传感器", font=("", 11, "bold")).pack(side="left")
        ttk.Checkbutton(top, text="显示静态信息", variable=self.show_static,
                        command=self._rebuild).pack(side="left", padx=(16, 8))
        ttk.Label(top, text="过滤:").pack(side="left", padx=(8, 2))
        entry = ttk.Entry(top, width=24, textvariable=self.filter_var)
        entry.pack(side="left")
        self.filter_var.trace_add("write", lambda *_: self._rebuild())

        frame = ttk.Frame(self.root, padding=(8, 0, 8, 0))
        frame.pack(fill="both", expand=True)
        self.tree = ttk.Treeview(frame, columns=("value", "unit", "status"), show="tree headings")
        self.tree.heading("#0", text="传感器", anchor="w")
        self.tree.heading("value", text="值", anchor="e")
        self.tree.heading("unit", text="单位", anchor="w")
        self.tree.heading("status", text="状态", anchor="w")
        self.tree.column("#0", width=320, stretch=True)
        self.tree.column("value", width=110, anchor="e", stretch=False)
        self.tree.column("unit", width=60, anchor="w", stretch=False)
        self.tree.column("status", width=90, anchor="w", stretch=False)
        vsb = ttk.Scrollbar(frame, orient="vertical", command=self.tree.yview)
        self.tree.configure(yscrollcommand=vsb.set)
        self.tree.pack(side="left", fill="both", expand=True)
        vsb.pack(side="right", fill="y")
        # 状态着色:等待首刷=橙,无驱动/不支持=灰,错误=红
        self.tree.tag_configure("wait", foreground="#b45309")
        self.tree.tag_configure("off", foreground="#8a8a8a")
        self.tree.tag_configure("err", foreground="#c62828")

        self.status = ttk.Label(self.root, padding=(8, 4), anchor="w")
        self.status.pack(fill="x")

    def _rebuild(self):
        """按(静态开关 × 过滤词)重建树,并立即填入当前值。"""
        flt = self.filter_var.get().strip().lower()
        self.tree.delete(*self.tree.get_children(""))
        self.leaf_rows.clear()
        cat_iid, dev_iid = None, None
        last_cat, last_dev = None, None
        for row in self.all_rows:
            if row.kind == SENSOR_STATIC and not self.show_static.get():
                continue
            if flt and flt not in f"{row.cat_name} {row.dev_name} {row.sen_name}".lower():
                continue
            if row.cat != last_cat:
                last_cat, last_dev = row.cat, None
                cat_iid = f"c{row.cat}"
                name = f"{row.cat_name}"
                self.tree.insert("", "end", iid=cat_iid, open=True, text=name)
            if row.key[1] != last_dev:
                last_dev = row.key[1]
                dev_iid = f"d{row.cat}.{row.key[1]}"
                self.tree.insert(cat_iid, "end", iid=dev_iid, open=True, text=row.dev_name)
            iid = f"r{row.key[0]}.{row.key[1]}.{row.key[2]}"
            self.tree.insert(dev_iid, "end", iid=iid, text=row.sen_name)
            self.leaf_rows[iid] = row
            text, st = read_cell(self.dll, row.key)
            self._set_row(iid, row, text, st)
        self._update_status(0, len(self.leaf_rows))

    def _set_row(self, iid, row, text, st):
        if st == 0:
            self.tree.set(iid, "value", text)
            self.tree.set(iid, "unit", row.unit)
            self.tree.set(iid, "status", "")
            self.tree.item(iid, tags=("" if row.kind == SENSOR_DYNAMIC else "off",))
        else:
            self.tree.set(iid, "value", "—")
            self.tree.set(iid, "unit", "")
            self.tree.set(iid, "status", STATUS_NAME.get(st, str(st)))
            tag = "wait" if st == 1 else ("off" if st in (8, 9) else "err")
            self.tree.item(iid, tags=(tag,))

    def _update_status(self, bad_count, total, seq=None):
        notes = []
        if bad_count:
            notes.append(f"⚠ {bad_count} 项未就绪")
        if not self.drv_ok:
            notes.append("ℹ 驱动增强层未加载:CPU 温度/主板传感器未列出;"
                         "以管理员运行可解锁(未装驱动先 --setup)")
        elif not self.mem_temp_ok:
            notes.append("ℹ 未列出内存温度:当前内存条不带温度传感器"
                         "(DDR4 为可选件;带传感器的条子以管理员运行即可见)")
        mode = "订阅推送" if self.stream is not None else "轮询读取"
        tag = f" · {mode}" + (f" · 批次#{seq}" if seq is not None else "")
        if notes:
            self.status.config(text=f"{datetime.now():%H:%M:%S} · " + ";".join(notes),
                               foreground="#b45309" if bad_count else "#666666")
        else:
            self.status.config(text=f"{datetime.now():%H:%M:%S} · ✓ 全部 {total} 项传感器读取正常{tag}",
                               foreground="#1a7f37")

    def _tick(self):
        if self.stream is not None:
            self._pump_stream()
        else:
            self._poll_cells()
        self.root.after(REFRESH_MS, self._tick)

    def _poll_cells(self):
        """轮询模式(退回路径):逐行读内部缓存。"""
        bad = 0
        for iid, row in self.leaf_rows.items():
            text, st = read_cell(self.dll, row.key)
            if st != 0:
                bad += 1
            self._set_row(iid, row, text, st)
        self._update_status(bad, len(self.leaf_rows))

    def _pump_stream(self):
        """订阅模式:取最新一批推送,只更新动态行(静态行不会变化)。"""
        sub, q = self.stream
        latest = None
        while True:
            try:
                latest = q.get_nowait()
            except queue.Empty:
                break
        if latest is None:
            return  # 无新批次,界面保持现状
        seq, items = latest
        vals = {k: (t, s) for k, t, s in items}
        bad = 0
        for iid, row in self.leaf_rows.items():
            if row.kind != SENSOR_DYNAMIC or row.key not in vals:
                continue
            text, st = vals[row.key]
            if st != 0:
                bad += 1
            self._set_row(iid, row, text, st)
        self._update_status(bad, len(self.leaf_rows), seq)

    def _on_close(self):
        unsubscribe_stream(self.dll, self.stream[0] if self.stream else None)
        self.dll.hwprobe_shutdown()
        self.root.destroy()

# ---------------- 控制台 JSON 模式 ----------------

def loop_json(dll):
    while True:
        n = C.c_uint32(0)
        dll.hwprobe_dump_json(HWPROBE_ALL_CATEGORIES, None, C.byref(n))
        buf = C.create_string_buffer(n.value)
        if dll.hwprobe_dump_json(HWPROBE_ALL_CATEGORIES, buf, C.byref(n)) == 0:
            print(cstr(buf.raw[:n.value - 1]), flush=True)
        time.sleep(REFRESH_MS / 1000)

# ---------------- 入口 ----------------

def run_tk(dll, show_static, use_stream=True):
    try:
        C.windll.shcore.SetProcessDpiAwareness(1)  # 高分屏清晰显示,失败则忽略
    except Exception:
        pass
    root = tk.Tk()
    try:
        rows = enumerate_tree(dll, include_static=True)
        Dashboard(root, dll, rows, show_static, use_stream=use_stream)
        root.mainloop()
    except Exception:
        messagebox.showerror("hwprobe", "界面异常,详见控制台输出")
        raise

def main():
    ap = argparse.ArgumentParser(description="hwprobe.dll Tk 硬件传感器仪表盘")
    ap.add_argument("--all", action="store_true", help="启动时显示静态信息传感器")
    ap.add_argument("--setup", action="store_true",
                    help="先一键安装 PawnIO 驱动(弹 UAC 静默安装,装一次即可)")
    ap.add_argument("--json", action="store_true", help="控制台 JSON 模式(每秒打印)")
    ap.add_argument("--stream", action="store_true",
                    help="控制台订阅推送模式:DLL 主动推送,本进程不轮询")
    ap.add_argument("--seconds", type=float, default=None,
                    help="配合 --stream:运行 N 秒后自动退出(默认 Ctrl+C 退出)")
    ap.add_argument("--no-stream", action="store_true",
                    help="Tk 仪表盘退回轮询读取模式(旧 DLL 或排查用)")
    args = ap.parse_args()

    dll = load_dll()
    bind(dll)

    abi = dll.hwprobe_abi_version()
    if abi < 2:
        print(f"警告: ABI 版本 {abi} < 2,无订阅推送接口(请重新构建 DLL);"
              f"将退回轮询模式", file=sys.stderr)

    if args.setup and not setup_driver(dll):
        return 1

    rc = dll.hwprobe_init()
    if rc != 0:
        print(f"hwprobe_init 失败:{STATUS_NAME.get(rc, rc)}  {last_error(dll)}",
              file=sys.stderr)
        return 1
    dll.hwprobe_set_poll_interval(REFRESH_MS)

    try:
        if args.json:
            loop_json(dll)
        elif args.stream:
            loop_stream(dll, args.seconds)
        else:
            run_tk(dll, args.all, use_stream=(not args.no_stream) and abi >= 2)
    except KeyboardInterrupt:
        pass
    finally:
        dll.hwprobe_shutdown()
    return 0

if __name__ == "__main__":
    sys.exit(main())
