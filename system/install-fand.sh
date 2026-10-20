#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════
# install-fand.sh —— 把风扇曲线守护装成 systemd 服务
#
# 为什么需要它：没有服务化时，曲线只是"手动 nohup 着的脚本" ——
#   关掉终端就没了、崩溃不会重启、重启机器就回到 BIOS 曲线、日志也不落盘。
#
# 用法
#   sudo bash install-fand.sh install     # 安装（两处二进制 + 服务 + 桌面启动器）+ 启动 + 开机自启
#   sudo bash install-fand.sh uninstall   # 停止 + 卸载
#   sudo bash install-fand.sh restart     # 重启服务
#   sudo bash install-fand.sh status      # 服务状态 + 当前风扇（不需要 root）
#        bash install-fand.sh logs        # 最近日志（不需要 root）
#        bash install-fand.sh follow      # 跟踪日志
# ═══════════════════════════════════════════════════════════════════════════
set -uo pipefail

UNIT=omen-fand.service
UNITDIR=/etc/systemd/system
BINDIR=/usr/local/bin

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)   # .../omen-control-rs/system
PROJ=$(dirname "$HERE")                              # .../omen-control-rs
TPL="$HERE/omen-fand.service.in"
OMEN_SYS=/sys/devices/platform/omen-wmi

R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; B=$'\033[1m'; N=$'\033[0m'
hdr()  { printf '\n%s%s%s\n' "$B" "$*" "$N"; }
ok()   { printf '  %s✓%s %s\n' "$G" "$N" "$*"; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$*"; }
bad()  { printf '  %s✗%s %s\n' "$R" "$N" "$*"; }
info() { printf '    %s\n' "$*"; }

need_root() {
    if [ "$(id -u)" != 0 ]; then
        bad "这一步需要 root：sudo bash $0 $1"
        exit 1
    fi
}

# 服务以哪个用户跑：优先 sudo 的发起者，回退到第一个普通用户
target_user() {
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        printf '%s' "$SUDO_USER"; return
    fi
    local u
    u=$(awk -F: '$3>=1000 && $3<65534 && $6 != "" && $6 != "/" {print $1; exit}' /etc/passwd)
    printf '%s' "${u:-root}"
}

user_home() { getent passwd "$1" 2>/dev/null | cut -d: -f6; }

# 选出**最新的**已编译二进制（release 优先，实在没有才用 debug）。
#
# ⚠⚠ 2026-10-11 实测血案：本函数原先只看 `$PROJ/target/release`，
#   而 `build.sh` 因为 NTFS 缺 POSIX 文件锁语义，会把 target **重定向**到
#   `$CARGO_TARGET_DIR`（默认 `~/.cache/omen-control-target`，见 build.sh）。
#   于是两个路径各留一份二进制 ⇒ 本脚本每次都用**仓库里那份旧货**覆盖系统里的新版本，
#   表现为"明明重新编译过，服务跑的却还是老逻辑"，且完全不报错。
#   （实测证据：仓库 target/release 与 /usr/local/bin 同为 md5 286ff06f，
#     而带修复的新构建在 ~/.cache 里是 e636e1db。）
#   修法：把这些路径都列进来，**按 mtime 取最新**；release 全无时才退回 debug。
pick_bin() {
    local u home group p best cands
    u=$(target_user)
    home=$(user_home "$u")
    for group in release debug; do
        cands=()
        [ -n "${CARGO_TARGET_DIR:-}" ] && cands+=("$CARGO_TARGET_DIR/$group/omen-control")
        # ⚠ 必须用**目标用户**的家目录，不能用 $HOME：
        #   本脚本要 root 跑，此时 $HOME=/root ⇒ "$HOME/.cache/omen-control-target" 指向
        #   /root（不存在）⇒ 候选集里只剩仓库的旧构建，于是**又把旧二进制装回去**。
        #   实测踩到（2026-10-11 09:59）：helper 已是新版，两个 omen-control 却仍是旧 md5。
        [ -n "$home" ] && cands+=("$home/.cache/omen-control-target/$group/omen-control")
        cands+=("$PROJ/target/$group/omen-control")
        best=""
        for p in "${cands[@]}"; do
            [ -x "$p" ] || continue
            if [ -z "$best" ] || [ "$p" -nt "$best" ]; then best="$p"; fi
        done
        [ -n "$best" ] && { printf '%s' "$best"; return 0; }
    done
    return 1
}

gen_unit() {   # $1=user $2=config_dir $3=bin
    sed -e "s|@USER@|$1|g" \
        -e "s|@CONFIG_DIR@|$2|g" \
        -e "s|@BIN@|$3|g" \
        -e "s|@PROJ@|$PROJ|g" \
        "$TPL" > "$UNITDIR/$UNIT"
}

# 确保桌面启动器存在、且 Exec 指向用户级二进制。
#
# ⚠⚠ 2026-10-11 实测血案：GUI 一直停在 10-09 的旧构建上
#   桌面启动器指向 ~/.local/bin/omen-control（用户级），而本脚本只装 /usr/local/bin。
#   两个副本各自演化 ⇒ 用户从应用菜单点开的**永远是最旧的那个**，
#   表现为「新加的开关/配置项压根不出现、控件看着像不可调」，
#   而且完全没有任何报错（"程序能跑"与"跑的是最新版"是两件事）。
ensure_desktop() {   # $1=user  $2=home
    local d="$2/.local/share/applications/omen-control.desktop"
    local want="$2/.local/bin/omen-control"
    install -d -m 0755 -o "$1" -g "$1" "$2/.local/share/applications" 2>/dev/null || true
    if [ -f "$d" ]; then
        local changed=0
        grep -q "^Exec=$want$" "$d" 2>/dev/null || { sed -i "s|^Exec=.*|Exec=$want|" "$d"; changed=1; }
        grep -q "^Name=OMEN 控制面板$" "$d" 2>/dev/null || { sed -i "s|^Name=.*|Name=OMEN 控制面板|" "$d"; changed=1; }
        sed -i "s|^Comment=.*|Comment=HP OMEN 性能档 / 风扇曲线 / 温度限制|" "$d" 2>/dev/null && changed=1
        # 老版本写的是 Icon=power-manager（系统默认图标）；换成我们自己的
        grep -q "^Icon=omen-control$" "$d" 2>/dev/null || { sed -i "s|^Icon=.*|Icon=omen-control|" "$d" 2>/dev/null; changed=1; }
        grep -q "^StartupWMClass=" "$d" 2>/dev/null || { printf 'StartupWMClass=org.rosary.OmenControl\n' >> "$d"; changed=1; }
        chown "$1:$1" "$d" 2>/dev/null || true
        if [ "$changed" = 1 ]; then ok "已修正桌面启动器 → $want"; else ok "桌面启动器已是最新指向"; fi
    else
        cat > "$d" <<DESKTOP
[Desktop Entry]
Name=OMEN 控制面板
Comment=HP OMEN 性能档 / 风扇曲线 / 温度限制
Exec=$want
Icon=omen-control
StartupWMClass=org.rosary.OmenControl
Terminal=false
Type=Application
Categories=System;
DESKTOP
        chown "$1:$1" "$d" 2>/dev/null || true
        ok "已创建桌面启动器 → $want"
    fi

    # 应用图标：装进**用户级** hicolor 主题（与 .desktop 的 Icon=omen-control 对应）。
    # 用户级图标优先于 /usr/share/icons，所以源码安装也能立刻换掉系统默认那个。
    local idir="$2/.local/share/icons/hicolor/scalable/apps"
    local isrc="$PROJ/packaging/omen-control.svg"
    install -d -m 0755 -o "$1" -g "$1" "$idir" 2>/dev/null || true
    if [ -f "$isrc" ]; then
        if install -m 0644 -o "$1" -g "$1" "$isrc" "$idir/omen-control.svg" 2>/dev/null; then
            ok "已安装图标 → $idir/omen-control.svg"
        fi
    else
        warn "找不到图标源文件 $isrc（应用菜单会显示通用占位图）"
    fi
    command -v update-desktop-database >/dev/null 2>&1 && \
        sudo -u "$1" update-desktop-database "$2/.local/share/applications" 2>/dev/null || true
}

show_shutdown_note() {
    cat <<'EOF'
    提示：
      · 关掉面板 / 关掉终端都不会影响它 —— 它在 systemd 里跑，开机自动启动。
      · 停止或卸载服务时，daemon 会「写 0 + unthrottle 1→0」把风扇交回 BIOS。
      · 想看它实时在做什么：journalctl -u omen-fand -f
EOF
}

# ─────────────────────────────────────────────────────────────── install
do_install() {
    need_root install

    # ── 冲突检测：旧的分级守护会与本守护抢同一批硬件 ──
    # omen-thermal 自己也会写风扇（fan_unthrottle，两态）与 CPU 上限 —— 与本守护的
    # 连续曲线/限制策略写的是同一组节点，同时跑必然互相覆盖。
    # 本项目已把它的能力整合进本守护（见 docs/功能总清单与整合设计.md），它应当退役。
    for _s in omen-thermal.service thermoguard.service gpu-thermoguard.service; do
        if systemctl list-unit-files "$_s" 2>/dev/null | grep -q "^$_s"; then
            printf '  ! 检测到 %s —— 它会与本守护抢风扇/CPU 上限，建议退役：\n' "$_s"
            printf '      sudo systemctl disable --now %s\n' "$_s"
        fi
    done

    hdr "① 环境检查"
    if [ ! -f "$TPL" ]; then
        bad "找不到 unit 模板：$TPL"; exit 1
    fi
    ok "unit 模板：$TPL"

    if [ -e "$OMEN_SYS/fan_speed" ]; then
        ok "omen-wmi 已加载（$OMEN_SYS/fan_speed 存在）"
    else
        bad "omen-wmi 未加载 —— 先装内核模块："
        info "cd \"$PROJ/../omen-wmi-omen\" && sudo bash install.sh --build"
        exit 1
    fi

    local u h cfg bin
    u=$(target_user)
    h=$(user_home "$u")
    if [ -z "$h" ]; then bad "取不到用户 $u 的家目录"; exit 1; fi
    cfg="$h/.config/omen-control"
    ok "服务将以用户 $u 运行（配置目录 $cfg）"

    if [ ! -f "$cfg/config" ]; then
        warn "还没有配置文件 $cfg/config —— 先打开一次面板，或用 --fan-daemon --curve 试"
    fi

    bin=$(pick_bin) || {
        bad "找不到已编译的 omen-control —— 先构建（必须用 build.sh，它会处理 NTFS 的 target 重定向）："
        info "cd \"$PROJ\" && bash build.sh"
        exit 1
    }
    ok "二进制：$bin"

    hdr "② 安装二进制"
    install -m 0755 "$bin" "$BINDIR/omen-control"
    ok "$BINDIR/omen-control（服务与本面板共用）"

    # 桌面启动器（.desktop）指向的是**用户级** ~/.local/bin —— 必须一起更新。
    # 只装 /usr/local/bin 的话，从应用菜单点开的就是上次留下的旧版本。
    _home=$(user_home "$u")
    if [ -n "$_home" ] && [ -d "$_home" ]; then
        install -d -m 0755 -o "$u" -g "$u" "$_home/.local/bin"
        install -m 0755 "$bin" "$_home/.local/bin/omen-control"
        ok "$_home/.local/bin/omen-control（桌面启动器用）"
        ensure_desktop "$u" "$_home"
    else
        warn "拿不到 $u 的家目录，跳过桌面启动器的同步"
    fi

    hdr "③ 安装文档（unit 的 Documentation 指向它）"
    # ⚠ 不能指向项目里的 README：项目路径含中文，systemd 会报 Invalid URL
    install -d -m 0755 /usr/local/share/doc/omen-fand
    install -m 0644 "$HERE/README.md" /usr/local/share/doc/omen-fand/README.md
    ok "/usr/local/share/doc/omen-fand/README.md"

    hdr "④ 生成 unit"
    gen_unit "$u" "$cfg" "$BINDIR/omen-control"
    ok "$UNITDIR/$UNIT 已生成"

    if command -v systemd-analyze >/dev/null 2>&1; then
        if systemd-analyze verify "$UNITDIR/$UNIT" >/tmp/fand-verify.log 2>&1; then
            ok "systemd-analyze verify 通过"
        else
            warn "systemd-analyze verify 有输出（多半是无害的警告）："
            sed 's/^/       /' /tmp/fand-verify.log | head -6
        fi
        rm -f /tmp/fand-verify.log
    fi

    hdr "⑤ 启动"
    systemctl daemon-reload
    systemctl enable "$UNIT" >/dev/null 2>&1 && ok "已设为开机自启" || warn "enable 失败（可稍后重试）"
    if systemctl restart "$UNIT"; then ok "服务已启动"; else bad "启动失败"; fi

    sleep 3
    hdr "⑥ 状态"
    systemctl --no-pager -l status "$UNIT" 2>&1 | head -14 | sed 's/^/    /'
    hdr "⑦ 当前风扇"
    info "$(cat "$OMEN_SYS/fan_rpm" 2>/dev/null | tr '\n' ' ')"
    show_shutdown_note
}

# ───────────────────────────────────────────────────────────── uninstall
do_uninstall() {
    need_root uninstall
    hdr "① 停止服务"
    if systemctl is-active --quiet "$UNIT" 2>/dev/null; then
        systemctl stop "$UNIT" && ok "已停止（风扇应交回 BIOS）"
    else
        info "服务本来就没在跑"
    fi

    hdr "② 关闭自启并删除 unit"
    systemctl disable "$UNIT" >/dev/null 2>&1 && ok "已关闭开机自启" || info "disable 无事可做"
    rm -f "$UNITDIR/$UNIT" && ok "已删除 $UNITDIR/$UNIT"
    systemctl daemon-reload
    systemctl reset-failed "$UNIT" >/dev/null 2>&1 || true
    ok "daemon-reload 完成"

    hdr "③ 收尾确认"
    info "保留 $BINDIR/omen-control 与 ~/.local/bin/omen-control（面板还在用；要删自己 rm）"
    info "当前风扇：$(cat "$OMEN_SYS/fan_rpm" 2>/dev/null | tr '\n' ' ')"
    info "若转速没回落，等 60~70 秒；仍不降就手动跑："
    info "  echo 0 | sudo tee $OMEN_SYS/fan_speed"
    info "  echo 1 | sudo tee $OMEN_SYS/fan_unthrottle; sleep 1"
    info "  echo 0 | sudo tee $OMEN_SYS/fan_unthrottle"
}

# ────────────────────────────────────────────────────────────── status
do_status() {
    hdr "服务"
    systemctl --no-pager -l status "$UNIT" 2>&1 | head -16 | sed 's/^/  /'
    hdr "风扇（当前）"
    printf '  %s\n' "$(cat "$OMEN_SYS/fan_rpm" 2>/dev/null | tr '\n' ' ')"
    if [ -f "$OMEN_SYS/fan_speed" ]; then
        printf '  fan_speed: %s\n' "$(cat "$OMEN_SYS/fan_speed" | tr '\n' ' ')"
    fi
    hdr "最近 12 条日志"
    journalctl -u omen-fand -n 12 --no-pager 2>/dev/null | sed 's/^/  /' || info "（读不到 journal）"
}

do_logs()   { journalctl -u omen-fand -n 80 --no-pager 2>/dev/null | sed 's/^/  /'; }
do_follow() { printf '跟踪日志（Ctrl-C 退出；不会影响服务）\n'; journalctl -u omen-fand -f; }

# ─────────────────────────────────────────────────────────────── main
case "${1:-}" in
    install)   do_install ;;
    uninstall) do_uninstall ;;
    restart)   need_root restart; systemctl restart "$UNIT" && ok "已重启" || bad "重启失败" ;;
    status)    do_status ;;
    logs)      do_logs ;;
    follow)    do_follow ;;
    *)
        cat <<EOF
用法：sudo bash $0 {install|uninstall|restart}

  install     安装 + 启动 + 开机自启
  uninstall   停止 + 卸载
  restart     重启服务

不需要 root 的：
  status      服务状态 + 当前风扇
  logs        最近 80 条日志
  follow      跟踪日志

unit 模板：$TPL
项目根目录：$PROJ
EOF
        ;;
esac
