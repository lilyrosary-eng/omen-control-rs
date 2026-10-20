#!/usr/bin/env bash
# 构建 + 安装 OMEN 安全控制面板 (Rust + GTK4/libadwaita)
#
# 三种模式:
#   ./build.sh            仅编译
#   ./build.sh --install  编译 + 装到 ~/.local/bin + 应用菜单 + 开机自启（源码安装）
#   ./build.sh --deb      编译 + 打成 dist/omen-control_<版本>_<架构>.deb（推荐）
#
# 为什么推荐 --deb：源码安装会把二进制复制到多处（~/.local/bin、/usr/local/bin…），
# 一旦有旧副本就会"看着改了、其实跑的是旧货"。deb 只留一份 /usr/bin/omen-control，
# 版本号由 dpkg 统一管理，`dpkg -s omen-control` 与 `omen-control --version` 互相印证。
#
# 依赖: cargo/rustc、GTK4 与 libadwaita 的开发包（打包另需 dpkg-deb，系统自带）
set -euo pipefail

# 在 NTFS(D盘) 上，执行位可能因复制/挂载丢失；先自行补上，
# 这样 "./build.sh" 与 "bash build.sh" 两种用法都能工作。
[[ -x "${BASH_SOURCE[0]}" ]] || chmod +x "${BASH_SOURCE[0]}" 2>/dev/null || true

PROJ_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DO_INSTALL=0
DO_DEB=0
case "${1:-}" in
  --install) DO_INSTALL=1 ;;
  --deb)     DO_DEB=1 ;;
esac

# 关键：源码在 NTFS（D 盘）上时，cargo 不能在 NTFS 里建 target/。
# NTFS 缺少 POSIX 语义，编译会出现 "failed to remove ... Operation not permitted"、
# 文件锁失效、耗时暴增等问题。改为把 target 放到 Linux 原生文件系统。
# 注意：ntfs3 下 `stat -f -c %T` 返回 "UNKNOWN (0x...)"，mountinfo 又是 " - ntfs3 " 格式，
# 都不可靠；这里用 findmnt -o FSTYPE 取本目录所在文件系统的类型，最稳。
FSTYPE="$(findmnt -n -o FSTYPE -T "$PROJ_DIR" 2>/dev/null || echo unknown)"
case "$FSTYPE" in
  ntfs|ntfs3|fuseblk)
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/omen-control-target}"
    echo "[提示] 源码位于 $FSTYPE 文件系统(D盘)，target 目录改放到:"
    echo "        $CARGO_TARGET_DIR"
    echo "      （NTFS 不适合作为编译输出目录，否则易出现文件锁/权限错误）"
    ;;
esac

echo "==> 1/3 检查工具链"
if ! command -v cargo >/dev/null 2>&1; then
  echo "缺少 cargo。安装方式二选一："
  echo "  A) Ubuntu 包（推荐，国内镜像快）: sudo apt install cargo rustc"
  echo "  B) 官方 rustup（国内极慢）: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  exit 1
fi
cargo --version
rustc --version

echo "==> 2/3 检查 GTK4 / libadwaita 开发包"
# 用 pkg-config 判断（比 dpkg -s 更准：直接反映头文件/pkgconfig 是否齐全，且快得多）
MISSING=()
need_pc() { pkg-config --exists "$1" >/dev/null 2>&1 || MISSING+=("$2"); }
need_pc gtk4         libgtk-4-dev
need_pc libadwaita-1 libadwaita-1-dev
need_pc glib-2.0     libglib2.0-dev
pkg-config --exists pkg-config 2>/dev/null || MISSING+=("pkg-config")
if (( ${#MISSING[@]} > 0 )); then
  echo "缺少开发包: ${MISSING[*]}"
  echo "请执行: sudo apt install ${MISSING[*]}"
  exit 1
fi
echo "GTK4 $(pkg-config --modversion gtk4) / libadwaita $(pkg-config --modversion libadwaita-1) 就绪"

echo "==> 3/3 编译 (release)"
cd "$PROJ_DIR"

# ~/.cargo 若属主异常（例如被 root 创建），cargo 写 registry 缓存会报
# "Permission denied"。此时把 CARGO_HOME 换到可写目录，并继承已有镜像配置。
if [[ ! -w "${CARGO_HOME:-$HOME/.cargo}" ]]; then
  NEW_CARGO_HOME="$HOME/.cargo-local"
  echo "[修复] \$CARGO_HOME 不可写，改用: $NEW_CARGO_HOME"
  mkdir -p "$NEW_CARGO_HOME"
  if [[ -f "$HOME/.cargo/config.toml" && ! -f "$NEW_CARGO_HOME/config.toml" ]]; then
    cp "$HOME/.cargo/config.toml" "$NEW_CARGO_HOME/config.toml" \
      && echo "        已继承镜像配置（避免退回官方源导致卡住）"
  fi
  export CARGO_HOME="$NEW_CARGO_HOME"
fi

# 注入构建时间戳：`omen-control --version` 与守护启动横幅都会打印它。
# 这是回答"我改的到底编进去没有"最直接的证据 —— 不必再靠比对 md5 猜。
export OMEN_BUILD_TS="${OMEN_BUILD_TS:-$(date '+%F %T %z')}"
echo "构建时间戳: $OMEN_BUILD_TS"

cargo build --release

BIN="${CARGO_TARGET_DIR:-$PROJ_DIR/target}/release/omen-control"
[[ -x "$BIN" ]] || { echo "[失败] 未生成可执行文件: $BIN"; exit 1; }
echo "构建完成: $BIN"
"$BIN" --check || true

if (( DO_INSTALL )); then
  echo
  echo "==> 安装到应用菜单 + 开机自启"
  "$BIN" --install
  echo
  echo "完成。在应用菜单搜「OMEN」即可打开；下次登录会自动启动。"
elif (( DO_DEB )); then
  echo
  echo "==> 打包 deb"
  bash "$PROJ_DIR/packaging/make-deb.sh" "$BIN" "" "$OMEN_BUILD_TS"
else
  echo
  echo "提示: 直接运行 $BIN 启动面板；"
  echo "      加 --deb 打成安装包（推荐），加 --install 装到 ~/.local/bin。"
fi

# 免密状态提示（deb 装到 /usr/libexec，历史脚本装到 /usr/local/libexec，两个都认）
echo
if [[ -x /usr/libexec/omen-control/omen-control-helper || -x /usr/local/libexec/omen-control-helper ]]; then
  echo "[免密] 已配置：改档位不会弹密码框。"
else
  echo "[免密] 尚未配置：每次改档位会弹一次授权框。"
  echo "       deb 包会自动配置；源码安装请执行一次："
  echo "         sudo bash \"$PROJ_DIR/system/setup-nopasswd.sh\""
fi

