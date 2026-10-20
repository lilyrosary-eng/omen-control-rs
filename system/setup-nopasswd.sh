#!/bin/bash
# ============================================================================
# 一次性配置：让 OMEN 控制面板改性能档 / 风扇开关时**不再弹密码框**
#
# 做法：安装一个白名单 helper，并在 /etc/sudoers.d/ 里只对这一个脚本免密。
#      不放开任意命令，不新增系统服务，不修改内核模块。
#
# 用法（必须 root）：
#   sudo bash setup-nopasswd.sh              # 安装 / 更新
#   sudo bash setup-nopasswd.sh --uninstall  # 卸载，恢复成"每次输密码"
#
# ⚠ **用 deb 包安装的话不需要跑这个** —— postinst 已经写好了同样的规则
#   （区别只在于它按组 `%sudo` 授权，本脚本按单个用户授权，二者都可用）。
#   本脚本保留给"从源码安装"的场景，且刻意装到与 deb 相同的位置，
#   避免出现两份 helper（见下面 HELPER_DST 的说明）。
# ============================================================================

set -euo pipefail

HELPER_SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/omen-control-helper"
# ⚠ 这个路径**必须与 deb 包一致**（deb 装在 /usr/libexec/omen-control/）。
#   2026-10-11 之前这里装的是 /usr/local/libexec/omen-control-helper，于是系统上
#   会有**两份 helper**，而免密规则只放行其中一份 ⇒ 用旧路径调用会得到
#   "interactive authentication is required"，看着像"免密没配好"，极难定位（实测踩到）。
HELPER_DST="/usr/libexec/omen-control/omen-control-helper"
SUDOERS_FILE="/etc/sudoers.d/omen-control"

die() { echo "[错误] $*" >&2; exit 1; }
info() { echo "[信息] $*"; }
ok() { echo "[完成] $*"; }

[ "$(id -u)" -eq 0 ] || die "请用 sudo 运行：sudo bash $0"

# 目标用户：优先取 sudo 调用者，回退到 logname / SUDO_USER
TARGET_USER="${SUDO_USER:-}"
[ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ] || TARGET_USER="$(logname 2>/dev/null || true)"
[ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ] || die "无法确定目标用户，请用 sudo -u 之外的方式指定"

# ---------------------------------------------------------------- 卸载分支
if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$SUDOERS_FILE" "$HELPER_DST"
    ok "已卸载：$SUDOERS_FILE 与 $HELPER_DST"
    info "控制面板仍可用，但每次改档位会恢复弹出密码框（pkexec）。"
    exit 0
fi

[ -f "$HELPER_SRC" ] || die "找不到 helper 源文件: $HELPER_SRC"

# ---------------------------------------------------------------- 1. 装 helper
info "1/4 安装 helper 到 $HELPER_DST"
install -d -o root -g root -m 0755 /usr/libexec/omen-control
install -o root -g root -m 0755 "$HELPER_SRC" "$HELPER_DST"

# 顺手清掉旧位置的副本：留着它只会制造"两个 helper、只有一个被放行"的静默失败。
if [ -e /usr/local/libexec/omen-control-helper ]; then
    rm -f /usr/local/libexec/omen-control-helper
    info "已删除旧位置的 helper（/usr/local/libexec/omen-control-helper）"
fi

# 关键安全校验：helper 绝不能被非 root 改写
OWNER="$(stat -c '%U:%G' "$HELPER_DST")"
PERM="$(stat -c '%a' "$HELPER_DST")"
[ "$OWNER" = "root:root" ] || die "helper 属主异常: $OWNER（必须 root:root）"
case "$PERM" in
    *[2367]|*[2367][0-9]|*[0-9][2367]) die "helper 权限含写位给非属主: $PERM" ;;
esac
ok "helper 就位（$OWNER $PERM）"

# ---------------------------------------------------------------- 2. 写 sudoers
info "2/4 写入免密规则（仅限该 helper）"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
cat > "$TMP" <<EOF
# OMEN 控制面板：免密执行专用白名单 helper。
# 仅放开这一个程序；其参数由 helper 自身校验，不接受任意命令。
$TARGET_USER ALL=(root) NOPASSWD: $HELPER_DST
EOF

# 先做语法校验，再落盘——避免写坏 sudo 配置导致无法提权
if ! visudo -cf "$TMP" >/dev/null 2>&1; then
    echo "--- 生成的内容 ---" >&2
    cat "$TMP" >&2
    die "sudoers 语法校验失败，已中止（未改动系统）"
fi
install -o root -g root -m 0440 "$TMP" "$SUDOERS_FILE"
ok "规则就位：$SUDOERS_FILE（属主 $TARGET_USER）"

# ---------------------------------------------------------------- 3. 全局校验
info "3/4 校验 sudoers 整体语法"
if ! visudo -c >/dev/null 2>&1; then
    rm -f "$SUDOERS_FILE"
    die "sudoers 整体校验失败，已回滚删除 $SUDOERS_FILE"
fi
ok "sudoers 语法正常"

# ---------------------------------------------------------------- 4. 实测
info "4/4 以 $TARGET_USER 身份实测免密执行"
# helper 无参数时以退出码 2 报用法；若免密未生效，sudo 会以非 0 退出（且压根不执行 helper）。
# 所以「返回 2」正是免密通道已打通的证据。
RC="$(sudo -u "$TARGET_USER" sudo -n "$HELPER_DST" >/dev/null 2>&1; echo $?)"
if [ "$RC" -eq 2 ] || [ "$RC" -eq 0 ]; then
    ok "免密通道已打通（$TARGET_USER 无需密码即可调用 helper）"
else
    echo "[警告] 实测返回码 $RC，免密可能未生效。" >&2
    echo "       请检查：$TARGET_USER 是否属于 sudo 组。" >&2
    exit 5
fi

echo
ok "全部完成。现在打开控制面板点档位，不会再弹密码框。"
info "回退方法：sudo bash $0 --uninstall"
