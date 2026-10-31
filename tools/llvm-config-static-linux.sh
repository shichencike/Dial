#!/bin/sh
# ============================================================================
# Linux/musl 上的「只静态」llvm-config 垫片（Termux aarch64 全静态路线必备）
#
# 背景（这是踩过坑才确认的关键点）：
#   dialc 依赖 inkwell，其特性 `llvm18-1-prefer-dynamic` 会启用 llvm-sys 的
#   `prefer-dynamic`，于是 LinkingPreferences::init() 得到 prefer_static=false，
#   get_link_libraries() 会**主动优先**尝试 `--libnames --link-shared`，
#   从而把 LLVM 动态链进来（libLLVM.so）。那样的二进制在 Termux 上根本跑不起来
#   ——Termux 既没有 glibc 也没有 musl 的动态加载器，执行时直接 "not found"。
#   （llvm-sys 并**没有** LLVM_SYS_<ver>_STATIC 这类环境变量可以覆盖它。）
#
# 对策：
#   本垫片对 `--libnames --link-shared` **以非零码退出**。llvm-sys 的
#   llvm_config_ex() 会把非零状态视为 Err，get_link_libraries() 随即回退到
#   `--link-static` 分支，于是锁死静态链接（libLLVM.a / 各组件 .a）。
#
# 用法：
#   LLVM_SYS_181_PREFIX=<dir>，且 <dir>/bin/llvm-config 为本脚本（需可执行）。
#   真实 llvm-config 由 LLVM_CONFIG_REAL 指定（默认 llvm-config-18）。
# ============================================================================
set -u

REAL="${LLVM_CONFIG_REAL:-llvm-config-18}"
LIBDIR="${LLVM_STATIC_LIBDIR:-}"

if [ -z "$LIBDIR" ]; then
    LIBDIR="$("$REAL" --libdir 2>/dev/null || true)"
fi

case "${1:-}" in
    --libnames)
        case "${2:-}" in
            --link-shared)
                # 拒绝动态链接 —— 迫使 llvm-sys 回退到静态分支
                echo "llvm-config shim: refusing --link-shared (static-only)" >&2
                exit 1
                ;;
            *)
                # 只输出 basename：extract_library()（Static 分支）会去掉
                # "lib" 前缀与 ".a" 后缀，再变成 -l<name> 交给链接器。
                if [ -n "$LIBDIR" ] && [ -f "$LIBDIR/libLLVM.a" ]; then
                    # 整包静态归档最稳：不存在组件库之间的循环依赖/顺序问题
                    echo "libLLVM.a"
                else
                    out="$("$REAL" --link-static --libnames 2>/dev/null || true)"
                    if [ -n "$out" ]; then
                        echo "$out"
                    else
                        found=$(ls "$LIBDIR"/libLLVM*.a 2>/dev/null || true)
                        if [ -n "$found" ]; then
                            echo "$found" | xargs -n1 basename
                        fi
                    fi
                fi
                ;;
        esac
        ;;
    --system-libs)
        # 静态分支下的系统库（libz/libzstd/libxml2/libffi 等）。
        #
        # 注意（坑）：llvm-sys 里系统库的链接种类取自
        #   sys_lib_kind = if cfg!(target_feature = "crt-static") { Static } else { Dynamic }
        # 而这个 cfg 是 **build script 自身（宿主）** 的编译特征，并非目标平台特征，
        # 于是这里即使整体 -static，-lffi 仍会被当作 dylib 链成 libffi.so.8，
        # 二进制因此在 Termux 上 "not found"。
        # 对策：改成 -l:<精确文件名>，直接指向静态归档（绕开 -Bdynamic 的 .so 偏好）。
        syslibs="$("$REAL" --link-static --system-libs)"
        ffi_a=""
        for f in /usr/lib/libffi.a /usr/local/lib/libffi.a /usr/lib/*/libffi.a; do
            if [ -f "$f" ]; then ffi_a="$f"; break; fi
        done
        if [ -n "$ffi_a" ]; then
            echo "$syslibs" | sed "s|-lffi|-l:$ffi_a|g"
        else
            # 没有静态归档就只能去掉：LLVM 的 C API 代码路径本身不依赖 libffi
            echo "$syslibs" | sed 's/-lffi//g'
        fi
        ;;
    *)
        # 其余查询（--version/--libdir/--cflags/--build-mode…）原样转发
        exec "$REAL" "$@"
        ;;
esac
