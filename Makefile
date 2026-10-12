# hwprobe Makefile —— 构建动态链接库,产物输出到 bin/dll/
#
# 用法(两个平台同一组目标):
#   make            # release 构建 → bin/dll/
#                    Windows: hwprobe.dll + 导入库 hwprobe.lib + C 头文件
#                    macOS:   libhwprobe.dylib + C 头文件
#   make debug      # debug 构建,同样输出到 bin/dll/
#   make clean      # 清空 bin/dll
#
# 环境要求:Rust(Windows 用 MSVC 工具链)+ GNU make;
# 在 Git Bash / MSYS2(Windows)或自带 make 的 macOS 终端里运行
# (cp、mkdir 等 recipe 命令依赖 sh)。

UNAME_S     := $(shell uname -s)
OUT_DIR     := bin/dll

ifeq ($(UNAME_S),Darwin)
# ---- macOS:cdylib 产出 libhwprobe.dylib,无导入库概念 ----
DYLIB       := libhwprobe.dylib
CARGO_PKG   := hwprobe-dll

.PHONY: all release debug clean

all: release

release:
	cargo build --release -p $(CARGO_PKG)
	@mkdir -p $(OUT_DIR)
	cp -f target/release/$(DYLIB) $(OUT_DIR)/
	cp -f include/hwprobe.h $(OUT_DIR)/
	@echo "==> 产物已输出到 $(OUT_DIR)/"

debug:
	cargo build -p $(CARGO_PKG)
	@mkdir -p $(OUT_DIR)
	cp -f target/debug/$(DYLIB) $(OUT_DIR)/
	cp -f include/hwprobe.h $(OUT_DIR)/
	@echo "==> 产物已输出到 $(OUT_DIR)/"

else
# ---- Windows:hwprobe.dll + 导入库 ----
DLL         := hwprobe.dll
IMPLIB      := hwprobe.dll.lib
IMPLIB_OUT  := hwprobe.lib   # 导入库改用单后缀名;DLL 名嵌在文件内部,改名不影响链接

.PHONY: all release debug clean

all: release

release:
	cargo build --release -p hwprobe-dll
	@mkdir -p $(OUT_DIR)
	cp -f target/release/$(DLL) $(OUT_DIR)/
	@if [ -f target/release/$(IMPLIB) ]; then cp -f target/release/$(IMPLIB) $(OUT_DIR)/$(IMPLIB_OUT); fi
	cp -f include/hwprobe.h $(OUT_DIR)/
	@echo "==> 产物已输出到 $(OUT_DIR)/"

debug:
	cargo build -p hwprobe-dll
	@mkdir -p $(OUT_DIR)
	cp -f target/debug/$(DLL) $(OUT_DIR)/
	@if [ -f target/debug/$(IMPLIB) ]; then cp -f target/debug/$(IMPLIB) $(OUT_DIR)/$(IMPLIB_OUT); fi
	cp -f include/hwprobe.h $(OUT_DIR)/
	@echo "==> 产物已输出到 $(OUT_DIR)/"
endif

clean:
	rm -rf $(OUT_DIR)
	@echo "==> 已清空 $(OUT_DIR)/"
