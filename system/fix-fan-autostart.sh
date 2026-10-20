#!/bin/bash
# 一次性修复：风扇守护「开不了 / 开机不自启」
#
# 用法（必须 root）：
#   sudo bash <此脚本>
#
# 它做五件事，对应实测到的三个断点：
#   ① 重建并按当前内核安装 hp-wmi-omen.ko
#      → 旧 .ko 只创建 11 个属性、独缺 fan_speed（源码是 12 项）
#   ② 确认 /sys/devices/platform/omen-wmi/fan_speed 真的存在
#      → 不存在就说明 .ko 仍是旧的，直接报出来，不做假成功
#   ③ 安装新版 omen-control-helper（它会自己 modprobe + 修属组，开关不再需要终端）
#   ④ 让 omen-fand 依赖 omen-wmi-build（内核升级后自动重建模块）
#   ⑤ 启用并启动 omen-fand，写后校验真实状态
set -uo pipefail

die() { echo "[错误] $*" >&2; exit 1; }
info() { echo "[信息] $*"; }
ok() { echo "[完成] $*"; }

[ "$(id -u)" -eq 0 ] || die "请用 sudo 运行：sudo bash $0"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CTRL="$(dirname "$HERE")"                      # omen-control-rs
# 模块仓库默认在同一级；也可显式指定 OMEN_KERNEL_REPO=/path/to/omen-wmi-omen
if [ -n "${OMEN_KERNEL_REPO:-}" ]; then
    WMI="$OMEN_KERNEL_REPO"
    [ -d "$WMI" ] || die "OMEN_KERNEL_REPO 指向的目录不存在：$WMI"
else
    WMI="$(cd "$CTRL/../omen-wmi-omen" 2>/dev/null && pwd)" \
        || die "找不到同级的 omen-wmi-omen 目录（可用 OMEN_KERNEL_REPO=... 指定）"
fi
NODE=/sys/devices/platform/omen-wmi/fan_speed
UNIT=/etc/systemd/system/omen-fand.service

echo "════════ 1/5 重建并安装内核模块 ════════"
bash "$WMI/install.sh" --build || die "模块构建/加载失败（看上面的输出）"

echo
echo "════════ 2/5 确认 fan_speed 节点 ════════"
for _ in $(seq 1 20); do [ -e "$NODE" ] && break; sleep 0.25; done
if [ ! -e "$NODE" ]; then
    echo "[错误] 模块加载了但没有 fan_speed —— 装进去的仍是旧版 .ko" >&2
    echo "        现有属性: $(ls /sys/devices/platform/omen-wmi/ 2>/dev/null | tr '\n' ' ')" >&2
    echo "        内核日志: $(dmesg 2>/dev/null | grep -E 'hp_omen: sysfs files created' | tail -1)" >&2
    exit 1
fi
ok "节点就绪：$NODE"

echo
echo "════════ 3/5 安装新版 helper ════════"
install -D -m 0755 "$HERE/omen-control-helper" \
    /usr/libexec/omen-control/omen-control-helper
# /usr/local 那份是历史副本；保持同步，避免排查时看到两个不同版本
[ -d /usr/local/libexec ] && \
    install -m 0755 "$HERE/omen-control-helper" /usr/local/libexec/omen-control-helper 2>/dev/null || true
ok "helper 已更新（自带 modprobe 自愈）"

# 面板二进制：开守护要 modprobe，耗时 1~3 秒；旧版在这期间会把开关拨回去
#（refresh 不检查 busy）⇒ 看起来像"点了就弹回"。新版已修，这里顺手装上。
NEWBIN="$CTRL/target/release/omen-control"
if [ -x "$NEWBIN" ] && [ -x /usr/bin/omen-control ] && [ "$NEWBIN" -nt /usr/bin/omen-control ]; then
    install -m 0755 "$NEWBIN" /usr/bin/omen-control
    ok "面板二进制已更新（重新打开面板窗口后生效）"
fi

echo
echo "════════ 4/5 让 omen-fand 依赖模块重建服务 ════════"
if [ ! -f "$UNIT" ]; then
    # 没装过服务：按模板生成（用户/配置目录/二进制取自当前环境）
    BIN=/usr/bin/omen-control
    [ -x "$BIN" ] || BIN="$CTRL/target/release/omen-control"
    sed -e "s|@USER@|${SUDO_USER:-$(id -un)}|g" \
        -e "s|@CONFIG_DIR@|/home/${SUDO_USER:-$(id -un)}/.config/omen-control|g" \
        -e "s|@BIN@|$BIN|g" \
        "$HERE/omen-fand.service.in" > "$UNIT"
    info "按模板生成了 $UNIT"
else
    grep -q '^Wants=omen-wmi-build.service' "$UNIT" || \
        sed -i '/^\[Unit\]/a Wants=omen-wmi-build.service' "$UNIT"
    sed -i 's|^After=.*|After=multi-user.target omen-wmi-build.service omen-wmi-perm.service|' "$UNIT"
    info "已补上 Wants/After=omen-wmi-build.service"
fi
systemctl daemon-reload
systemctl enable omen-wmi-build.service >/dev/null 2>&1 || true

echo
echo "════════ 5/5 启用并启动守护 ════════"
systemctl reset-failed omen-fand.service 2>/dev/null || true
systemctl enable --now omen-fand.service 2>&1 | sed 's/^/  /'
sleep 1

ACTIVE=$(systemctl is-active omen-fand.service 2>/dev/null)
ENABLED=$(systemctl is-enabled omen-fand.service 2>/dev/null)
echo
printf '  omen-fand : active=%s  enabled=%s\n' "$ACTIVE" "$ENABLED"
printf '  omen-wmi-build : active=%s  enabled=%s\n' \
    "$(systemctl is-active omen-wmi-build.service 2>/dev/null)" \
    "$(systemctl is-enabled omen-wmi-build.service 2>/dev/null)"

if [ "$ACTIVE" = "active" ] && [ "$ENABLED" = "enabled" ]; then
    echo
    ok "修好了：守护在跑，且已设为开机自启"
    echo "     风扇转速: $(cat /sys/devices/platform/omen-wmi/fan_rpm 2>/dev/null | tr '\n' ' ')"
    echo "     日志: journalctl -u omen-fand -n 20 --no-pager"
else
    echo
    echo "[错误] 服务仍未达到 active+enabled" >&2
    systemctl --no-pager status omen-fand.service 2>&1 | sed -n '1,12p' | sed 's/^/        /' >&2
    echo "        journalctl -u omen-fand -n 30 --no-pager" >&2
    exit 1
fi
