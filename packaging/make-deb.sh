#!/usr/bin/env bash
# ============================================================================
# 把 omen-control 打成 .deb —— 纯 dpkg-deb，**不依赖 cargo-deb、不需要联网**
#
# 用法：
#   bash packaging/make-deb.sh <已编译的可执行文件> [版本] [构建时间戳]
#   （一般不用直接调：`bash build.sh --deb` 会先编译好再调这里）
#
# 产物：dist/omen-control_<版本>_<架构>.deb
#
# ── 为什么值得打 deb（2026-10-11 的教训，不是"顺手美化"）──
#   之前同一个二进制散落在四处：/usr/local/bin、~/.local/bin、仓库 target/release、
#   ~/.cache/omen-control-target。于是：
#     · 每次都要靠 md5 猜"我改的到底跑没跑起来"（本轮连吃两次亏）；
#     · install 脚本互相覆盖（install-fand.sh 曾把仓库里的旧货装回系统）。
#   打进一个包之后：
#     · `dpkg -s omen-control` 给出**唯一**版本号；
#     · `omen-control --version` 给出**正在运行那份**的身份；
#     · 文件只有一份 /usr/bin/omen-control，PATH 上再没有第二份能盖住它。
# ============================================================================
set -euo pipefail

PROJ="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG=omen-control
BIN_SRC="${1:-}"
VERSION="${2:-}"
BUILD_TS="${3:-$(date '+%F %T %z')}"
ARCH="$(dpkg --print-architecture)"

# 内核模块仓库（默认与本身同级；可用 OMEN_KERNEL_REPO 指定）
KERNEL_REPO="${OMEN_KERNEL_REPO:-$PROJ/../omen-wmi-omen}"
DKMS_NAME=hp-wmi-omen
DKMS_VER=0.1.0
OUT_DIR="${OUT_DIR:-$PROJ/dist}"

die() { echo "[错误] $*" >&2; exit 1; }
info() { echo "[信息] $*"; }
ok() { echo "[完成] $*"; }

[ -n "$BIN_SRC" ] && [ -x "$BIN_SRC" ] || die "用法: bash packaging/make-deb.sh <可执行文件> [版本] [构建时间戳]"
if [ -z "$VERSION" ]; then
    VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$PROJ/Cargo.toml" | head -1)"
fi
[ -n "$VERSION" ] || die "拿不到版本号（Cargo.toml 里没有 version = \"x.y.z\"）"
command -v dpkg-deb >/dev/null 2>&1 || die "缺少 dpkg-deb（apt install dpkg）"

HELPER_SRC="$PROJ/system/omen-control-helper"
UNIT_TPL="$PROJ/packaging/omen-fand.service.in"
DESKTOP_SRC="$PROJ/packaging/omen-control.desktop"
TRAY_SRC="$PROJ/packaging/omen-tray"
TRAY_RESTART_SRC="$PROJ/packaging/omen-tray-restart"
TRAY_DESKTOP_SRC="$PROJ/packaging/omen-control-tray.desktop"
ICON_SRC="$PROJ/packaging/$PKG.svg"
for f in "$HELPER_SRC" "$UNIT_TPL" "$DESKTOP_SRC" "$TRAY_SRC" "$TRAY_RESTART_SRC" \
         "$TRAY_DESKTOP_SRC" \
         "$ICON_SRC" \
         "$PROJ/packaging/postinst" "$PROJ/packaging/prerm" "$PROJ/packaging/postrm"; do
    [ -f "$f" ] || die "找不到打包素材: $f"
done

# ── 依赖：从二进制自身的 NEEDED 反查所属 deb 包 ────────────────────────────
# 不硬编码 "libgtk-4-1" 之类的名字 —— 名字在内核/发行版之间会变，写错会直接
# 让 `dpkg -i` 因依赖不满足而拒绝安装。这里直接问 dpkg：这个 .so 属于哪个包。
resolve_deps() {
    local so path pkg out=""
    while read -r so; do
        [ -n "$so" ] || continue
        # ⚠ 必须 LC_ALL=C：本机 LANG=zh_CN.UTF-8 时 `ldd`/`readelf` 的
        #   "not found"/"共享库" 等字样会被翻译，字段位置也随之改变。
        path="$(LC_ALL=C ldd "$BIN_SRC" 2>/dev/null | awk -v s="$so" '$1==s {print $3}')"
        [ -n "$path" ] || continue
        pkg="$(dpkg -S "$(readlink -f "$path")" 2>/dev/null | head -1 | cut -d: -f1)" || true
        # 只保留真正的库包，且去重
        case "$pkg" in
            lib*|zlib*|libc6*) ;;
            *) continue ;;
        esac
        case ", $out, " in *", $pkg, "*) continue ;; esac
        out="${out:+$out, }$pkg"
    # ⚠⚠ 这里曾经是 `awk '... gsub(/[]]/,"",$5); print $5'`，**静默失效**：
    #   中文 locale 下 readelf 输出为 `0x1 (NEEDED) 共享库：[libc.so.6]`，
    #   只有 3 个字段，`$5` 是空的 ⇒ 循环一次都没进 ⇒ 永远走"反查失败"兜底分支。
    #   （因为兜底列表恰好和正确结果一样，这个 bug 藏了整轮都没被发现。）
    #   修法：LC_ALL=C + 按方括号内容提取，不依赖字段序号。
    done < <(LC_ALL=C readelf -d "$BIN_SRC" 2>/dev/null \
             | awk '/NEEDED/{for(i=1;i<=NF;i++) if ($i ~ /^\[.*\]$/) {gsub(/[\[\]]/,"",$i); print $i}}')
    printf '%s' "$out"
}
DEPS="$(resolve_deps)"
[ -n "$DEPS" ] || { DEPS="libc6, libgtk-4-1, libadwaita-1-0, libglib2.0-0"; info "依赖反查失败，退回保守列表"; }
# 托盘（usr/libexec/omen-control/omen-tray）是 Python 脚本，反查 .so 查不到它的依赖，
# 这三条要显式写：python3-gi（gi 绑定）、gir1.2-gtk-3.0（GTK3）、
# gir1.2-ayatanaappindicator3-0.1（托盘本体）。缺任一托盘都起不来。
DEPS="$DEPS, python3-gi, gir1.2-gtk-3.0, gir1.2-ayatanaappindicator3-0.1"
info "依赖: $DEPS"

# ── 组装文件树 ─────────────────────────────────────────────────────────────
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# mktemp -d 给的是 0700。包内 `./` 这一项会带着这个权限进归档，
# 虽然 dpkg 对 `./` 是特殊处理（不会去 chmod 根目录），但没必要留这种可疑条目。
chmod 0755 "$STAGE"

install -D -m 0755 "$BIN_SRC"     "$STAGE/usr/bin/$PKG"
install -D -m 0755 "$HELPER_SRC"  "$STAGE/usr/libexec/$PKG/omen-control-helper"
# 托盘（Python 脚本）：与 helper 同放 libexec —— 它们都是"被别的入口调用"的实现细节，
# 不该出现在用户 PATH 上（避免有人手动跑出第二个图标）。
install -D -m 0755 "$TRAY_SRC"    "$STAGE/usr/libexec/$PKG/omen-tray"
# 托盘的"杀掉旧进程 + 用同一套会话环境拉起新的"脚本（postinst 升级时调它）。
install -D -m 0755 "$TRAY_RESTART_SRC" "$STAGE/usr/libexec/$PKG/omen-tray-restart"
install -D -m 0644 "$UNIT_TPL"    "$STAGE/usr/share/$PKG/omen-fand.service.in"
install -D -m 0644 "$DESKTOP_SRC" "$STAGE/usr/share/applications/$PKG.desktop"
# 托盘开机自启：/etc/xdg/autostart 是各桌面会话都会读的公共位置，登录即拉起。
install -D -m 0644 "$TRAY_DESKTOP_SRC" "$STAGE/etc/xdg/autostart/$PKG-tray.desktop"
# 应用图标：装到 hicolor 主题的 scalable/apps 下，文件名必须等于 .desktop 里的 Icon=
# （apt 的 gtk-update-icon-cache 触发器会自动刷新缓存，无需我们手工调）
install -D -m 0644 "$PROJ/packaging/$PKG.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/$PKG.svg"
install -D -m 0644 "$PROJ/README.md" "$STAGE/usr/share/doc/$PKG/README.md" 2>/dev/null \
    || install -D -m 0644 /dev/null "$STAGE/usr/share/doc/$PKG/README.md"
install -D -m 0644 "$PROJ/packaging/copyright" "$STAGE/usr/share/doc/$PKG/copyright"
install -D -m 0755 "$PROJ/packaging/postinst" "$STAGE/DEBIAN/postinst"
install -D -m 0755 "$PROJ/packaging/prerm"    "$STAGE/DEBIAN/prerm"
install -D -m 0755 "$PROJ/packaging/postrm"   "$STAGE/DEBIAN/postrm"

# ── 内核模块源码（DKMS 用）─────────────────────────────────────────────────
# 没有它，装完只有 GUI、没有风扇控制 —— 所以缺源码时直接失败，不产出残包。
if [ ! -f "$KERNEL_REPO/hp-wmi-omen.c" ] || [ ! -f "$KERNEL_REPO/dkms.conf" ]; then
    die "找不到内核模块源码：$KERNEL_REPO（用 OMEN_KERNEL_REPO=/path/to/omen-wmi-omen 指定）"
fi
DKMS_DIR="$STAGE/usr/src/$DKMS_NAME-$DKMS_VER"
install -D -m 0644 "$KERNEL_REPO/hp-wmi-omen.c" "$DKMS_DIR/hp-wmi-omen.c"
install -D -m 0644 "$KERNEL_REPO/Makefile"      "$DKMS_DIR/Makefile"
install -D -m 0644 "$KERNEL_REPO/dkms.conf"     "$DKMS_DIR/dkms.conf"
install -D -m 0755 "$KERNEL_REPO/install.sh"    "$DKMS_DIR/install.sh"
# 只读探测/诊断用；--genkey 在 Secure Boot 机器上要用到
install -D -m 0644 "$KERNEL_REPO/README.md"     "$DKMS_DIR/README.md" 2>/dev/null || true

# dkms 是硬依赖：没有它，模块源码只是躺在 /usr/src 里没人编译
DEPS="$DEPS, dkms"

SIZE="$(du -sk "$STAGE" | cut -f1)"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $ARCH
Maintainer: Rosary <lilyrosary0@gmail.com>
Installed-Size: $SIZE
Depends: $DEPS
Section: utils
Priority: optional
Description: HP OMEN 16-k0xxx 控制面板（风扇曲线 / 温度限制 / 监视）
 仅支持 HP OMEN 16-k0xxx（主板 8A1A）。其它机型内核模块会拒绝所有写操作。
 .
 附带 hp-wmi-omen 内核模块源码（DKMS 编译）：风扇曲线经 HP 私有 WMI 命令
 GM12 写 EC 的 SFS1/SFS2；其余设置（CPU 上限 / 睿频 / EPP / GPU 时钟锁）
 走内核官方接口，可逆。
 .
 附带系统托盘：登录后常驻顶栏，可切档位、开关风扇守护，显示 CPU 温度与风扇转速。
 .
 构建时间: $BUILD_TS'
EOF

# ── 打包 ───────────────────────────────────────────────────────────────────
mkdir -p "$OUT_DIR"
DEB="$OUT_DIR/${PKG}_${VERSION}_${ARCH}.deb"
# --root-owner-group：把整棵树的属主设成 root:root，因此**普通用户也能打包**，
# 不需要 fakeroot，也不会有"文件属主是打包者"的隐患（dpkg 1.19+ 支持）。
if dpkg-deb --help 2>/dev/null | grep -q -- "--root-owner-group"; then
    dpkg-deb --root-owner-group --build "$STAGE" "$DEB" >/dev/null
else
    die "dpkg 太旧，不支持 --root-owner-group（需要 dpkg 1.19+）"
fi

ok "已生成: $DEB"
echo
dpkg-deb --info "$DEB"
echo
echo "== 包内文件 =="
dpkg-deb --contents "$DEB"
echo
info "安装:   sudo apt install \"$DEB\"     （apt 会自动补依赖；dpkg -i 也行）"
info "卸载:   sudo apt remove $PKG"
info "核对版本: dpkg -s $PKG | head -3   ·   omen-control --version"