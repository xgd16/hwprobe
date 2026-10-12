//go:build windows

package main

import "syscall"

// winDLL 保活:持有 *syscall.DLL 引用,避免库对象被 GC 后句柄悬空。
var winDLL *syscall.DLL

// loadLibrary 用 LoadLibrary 加载 DLL(与 Unix 侧 dlopen 等价)。
// 传绝对路径即可定位;hwprobe.dll 无外部依赖,不存在依赖 DLL 搜索问题。
func loadLibrary(path string) (uintptr, error) {
	d, err := syscall.LoadDLL(path)
	if err != nil {
		return 0, err
	}
	winDLL = d
	return uintptr(d.Handle), nil
}

// findSymbol 查询导出符号(GetProcAddress),用于探测可选接口。
func findSymbol(handle uintptr, name string) (uintptr, error) {
	return syscall.GetProcAddress(syscall.Handle(handle), name)
}
