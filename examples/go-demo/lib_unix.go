//go:build !windows

package main

import "github.com/ebitengine/purego"

// loadLibrary 用 dlopen 加载动态库(Windows 侧见 lib_windows.go)。
// RTLD_NOW:加载即完成符号重定位,缺符号当场报错而不是调用时才崩。
func loadLibrary(path string) (uintptr, error) {
	return purego.Dlopen(path, purego.RTLD_NOW|purego.RTLD_GLOBAL)
}

// findSymbol 查询导出符号,用于探测可选接口(如 ABI 2 才有的订阅推送)。
func findSymbol(handle uintptr, name string) (uintptr, error) {
	return purego.Dlsym(handle, name)
}
