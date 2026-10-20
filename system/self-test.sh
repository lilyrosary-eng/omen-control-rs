#!/bin/bash
# ============================================================================
# OMEN 控制面板 —— 端到端功能自检
#
# 逐项**真实读写一次**，验证每个功能确实生效，然后立即恢复原值。
# 全部走内核官方接口，可逆。
#
# 测试期间会有短暂状态变化（CPU 上限、睿频、频率上限、EPP、风扇各会动一下），
# 结束时会全部恢复。风扇那一项可能短暂变响，属正常现象。
#
# 刻意**不测**的三项（都已在真机实测确认，不是遗漏）：
#   · ACPI 性能档：本机写下去对硬件无任何影响，只作只读报告；
#   · GPU 功耗墙：nvidia-smi -pl 返回 0 但实测为空操作，只作只读报告；
#   · CPU PL1 功耗墙：内核声明上限与 BIOS 实设值矛盾，且会被 thermald/ppd 改回，
#     只作只读报告。
#
# 调用路径与面板完全一致：`sudo -u <你的账号> sudo -n <helper> ...` —— 
# 因此这项自检同时也验证了免密通道。
#
# 用法：sudo bash self-test.sh
# ============================================================================
set -uo pipefail

[ "$(id -u)" -eq 0 ] || { echo "请用 sudo 运行：sudo bash $0"; exit 1; }

# 免密 helper 的位置：deb 包装到 /usr/libexec/omen-control/，
# 历史的 setup-nopasswd.sh 装到 /usr/local/libexec/。
# ⚠ 必须两个都找 —— 硬编码旧路径会让本自检在 deb 安装后**必然报免密失败**
#   （而免密规则只放行了新路径，失败原因看着像"权限没配好"，极难定位）。
HELPER=""
for _p in /usr/libexec/omen-control/omen-control-helper \
          /usr/local/libexec/omen-control-helper; do
  if [ -x "$_p" ]; then HELPER="$_p"; break; fi
done
[ -n "$HELPER" ] || HELPER=/usr/local/libexec/omen-control-helper
PSTATE=/sys/devices/system/cpu/intel_pstate
C0=/sys/devices/system/cpu/cpu0/cpufreq
PP=/sys/firmware/acpi/platform_profile
RAPL=/sys/class/powercap/intel-rapl:0
HWMON="$(ls -d /sys/devices/platform/hp-wmi/hwmon/hwmon* 2>/dev/null | head -1)"
USER_NAME="${SUDO_USER:-$(id -un)}"

PASS=0; FAIL=0; WARN=0
ok()   { printf "  ✅ %s\n" "$1"; PASS=$((PASS + 1)); }
bad()  { printf "  ❌ %s\n" "$1"; FAIL=$((FAIL + 1)); }
warn() { printf "  ⚠  %s\n" "$1"; WARN=$((WARN + 1)); }
step() { printf "\n【%s】\n" "$1"; }
eq()   { if [ "$2" = "$3" ]; then ok "$1 = $2"; else bad "$1：期望 $3，实际 $2"; fi; }

# 与面板一致的调用路径
h() { sudo -u "$USER_NAME" sudo -n "$HELPER" "$@"; }
# 判定 helper 是否成功（退出码 0）
hok() { h "$@" >/dev/null 2>&1; }

echo "OMEN 控制面板 · 端到端自检"
echo "════════════════════════════════════════════════"

# ---------------------------------------------------------------- 1
step "1 · 免密通道"
if [ ! -x "$HELPER" ]; then
    bad "helper 不存在：$HELPER（请先跑 system/setup-nopasswd.sh）"
    echo; echo "无法继续。"; exit 1
fi
# ⚠ 这个数字必须跟着 helper 一起改（2026-10-11 修正：原写 6，而 helper 早已是 8 ——
#   少算了后来加的 set-cpu-freq / set-rapl，还提到两个早就删掉的子命令，
#   于是本自检**每次都会误报**"比最新版多"并给出错误的回收建议）。
#   当前 8 个：set-maxfan / set-cpu-pct / set-epp / set-turbo /
#              set-cpu-freq / set-rapl / set-gpu-clk / set-fan-daemon
N=$(grep -cE '^  set-' "$HELPER")
if [ "$N" -eq 8 ]; then
    ok "helper 子命令数 = 8（最新版）"
elif [ "$N" -gt 8 ]; then
    # 多出命令不影响功能，但每一分 root 权限面都应能说清用途 —— 说不出就收回。
    warn "helper 子命令数 = $N，比最新版多（可能含已废弃的 set-profile / set-gpu-power / set-governor）"
    echo "     → 功能不受影响，但建议收回多余权限：sudo bash $(dirname "$0")/setup-nopasswd.sh"
else
    bad "helper 子命令数 = $N（不完整，功能会缺）"
    echo "     → 请重跑：sudo bash $(dirname "$0")/setup-nopasswd.sh"
fi
h >/dev/null 2>&1
RC=$?
if [ "$RC" -eq 2 ]; then
    ok "免密调用可用（无参数时返回用法码 2，说明确实以 $USER_NAME 身份免密执行）"
else
    bad "免密不可用（返回 $RC；期望 2）"
fi

# ---------------------------------------------------------------- 2
step "2 · CPU 性能上限（限功耗降温主力）"
O=$(cat $PSTATE/max_perf_pct); echo "     原值 $O"
if hok set-cpu-pct 80; then
    sleep 0.3; eq "写入 80" "$(cat $PSTATE/max_perf_pct)" "80"
else bad "写入命令失败"; fi
if hok set-cpu-pct "$O"; then
    sleep 0.3; eq "恢复 $O" "$(cat $PSTATE/max_perf_pct)" "$O"
else bad "恢复命令失败"; fi

# ---------------------------------------------------------------- 3
step "3 · 睿频开关"
O=$(cat $PSTATE/no_turbo); echo "     原值 $O（0=睿频开）"
if hok set-turbo off; then
    sleep 0.3; eq "关闭睿频" "$(cat $PSTATE/no_turbo)" "1"
else bad "关闭命令失败"; fi
if [ "$O" = "0" ]; then hok set-turbo on || bad "恢复命令失败"; else hok set-turbo off || bad "恢复命令失败"; fi
sleep 0.3; eq "恢复 $O" "$(cat $PSTATE/no_turbo)" "$O"

# ---------------------------------------------------------------- 4
step "4 · CPU 频率上限（24 核批量）"
O=$(cat $C0/scaling_max_freq); echo "     原值 $O kHz"
if hok set-cpu-freq 3000000; then
    sleep 0.3; eq "写入 3.0 GHz" "$(cat $C0/scaling_max_freq)" "3000000"
    N24=$(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_max_freq 2>/dev/null | sort -u | wc -l)
    [ "$N24" -eq 1 ] && ok "24 个核心均已同步" || bad "各核心频率上限不一致（$N24 种取值）"
else bad "写入命令失败"; fi
if hok set-cpu-freq "$O"; then
    sleep 0.3; eq "恢复 $O" "$(cat $C0/scaling_max_freq)" "$O"
else bad "恢复命令失败"; fi

# ---------------------------------------------------------------- 5
step "5 · 能效偏好 EPP"
O=$(cat $C0/energy_performance_preference); echo "     原值 $O"
if hok set-epp power; then
    sleep 0.3; eq "写入 power" "$(cat $C0/energy_performance_preference)" "power"
else bad "写入命令失败"; fi
if hok set-epp "$O"; then
    sleep 0.3; eq "恢复 $O" "$(cat $C0/energy_performance_preference)" "$O"
else bad "恢复命令失败"; fi

# ---------------------------------------------------------------- 6
step "6 · 内核 ACPI 档位（只读 · 本机不驱动硬件）"
# 真机实测：切换 cool/quiet/balanced/performance 后，EPP / 频率上限 / 睿频 /
# max_perf_pct / GPU 功耗墙全部不变，EC 0x95 恒为 0x30。因此不做写入测试，
# 也不在界面提供控件——只报告内核上报值。
echo "     内核上报：$(cat $PP 2>/dev/null || echo 读不到)"
echo "     ⏭ 本机主板 8A1A 未被内核 hp-wmi 识别，切换该档位对硬件无影响，故不测试。"

# ---------------------------------------------------------------- 7
step "7 · 最大风扇（可能短暂变响）"
O=$(cat $HWMON/pwm1_enable); echo "     原值 $O（2=自动）"
if hok set-maxfan on; then
    sleep 0.5; eq "开最大" "$(cat $HWMON/pwm1_enable)" "0"
else bad "开启命令失败"; fi
if hok set-maxfan off; then
    sleep 0.5; eq "回自动" "$(cat $HWMON/pwm1_enable)" "$O"
else bad "恢复命令失败"; fi

# ---------------------------------------------------------------- 8
step "8 · GPU（只读 · 功耗墙由 vBIOS 锁定）"
if [ -x /usr/bin/nvidia-smi ]; then
    cur_limit() {
        nvidia-smi -q -d POWER 2>/dev/null \
            | grep -m1 'Current Power Limit' \
            | sed 's/.*: *//; s/ *W.*//'
    }
    nvidia-smi --query-gpu=name,temperature.gpu,power.draw,power.max_limit,pstate \
        --format=csv,noheader,nounits 2>/dev/null | awk -F', ' '{
            printf "     GPU: %s\n     %s°C · 实时 %s W · %s · 硬件上限 %s W\n", $1, $2, $3, $5, $4 }'
    echo "     当前功耗墙（动态值，含 Dynamic Boost）：$(cur_limit) W"
    echo "     ⏭ 实测 nvidia-smi -pl 返回 0 但完全无效（写 40W 仍跑出 80W 峰值），"
    echo "       TGP 由平台 vBIOS 锁定，故不测试、界面也不提供控件。"
else
    echo "     ⏭ 未找到 nvidia-smi，跳过"
fi

# ---------------------------------------------------------------- 9
step "9 · CPU 功耗墙 PL1 + PL2（真实写入 → 读回 → 立即还原）"
# 2026-10-11 重写。旧版这里写的是「只读，不提供控件」，理由是"该值会被
# thermald / power-profiles-daemon 改回" —— **已被实测证伪**：写 30 W 后连续观测
# 3 分钟纹丝不动，期间 thermald 一句日志都没有；面板也早已提供可调控件。
# 另：必须连 PL2（short_term）一起写 —— PL1 是**约 56 秒窗口的平均值**，
# 只压 PL1 时 CPU 仍能按 PL2 的 135 W 连续睿频几十秒（实测前 24 秒等于没限）。
PL1_PATH=$RAPL/constraint_0_power_limit_uw
PL2_PATH=$RAPL/constraint_1_power_limit_uw
P1=$(( $(cat $PL1_PATH) / 1000000 ))
P2=$(( $(cat $PL2_PATH) / 1000000 ))
MAXV=$(( $(cat $RAPL/constraint_0_max_power_uw) / 1000000 ))
echo "     当前 PL1=${P1} W · PL2=${P2} W"
echo "     （constraint_0_max_power_uw 报 ${MAXV} W —— 只是 BIOS 能力值，不是钳位上限）"
TEST_W=30
if hok set-rapl "$TEST_W"; then
    eq "PL1 写入后读回" "$(( $(cat $PL1_PATH) / 1000000 ))" "$TEST_W"
    eq "PL2 写入后读回" "$(( $(cat $PL2_PATH) / 1000000 ))" "$TEST_W"
    if hok set-rapl "$P1"; then
        ok "已还原 PL1/PL2 = ${P1} W"
    else
        bad "还原失败（请手动：$HELPER set-rapl $P1）"
    fi
else
    bad "set-rapl 写入失败（免密通道或 helper 有问题）"
fi

# ---------------------------------------------------------------- 10
step "10 · GPU 频率锁定（本项决定能否恢复 GPU 降温能力）"
# 背景：本机 GPU 功耗墙（nvidia-smi -pl）实测是空操作，TGP 被 vBIOS 锁定。
# 但"锁 GPU 频率"（-lgc）是**另一条独立路径**，且需要 root（本自检正好是 root 跑）。
# 厂商工具 OGH/OMEN 的"性能模式"本质上就是提高 GPU 频率/TGP，所以这条路值得试。
# 本项只探测：锁定 → 读回 → 立刻复位，失败不影响面板其它功能。
if [ -x /usr/bin/nvidia-smi ]; then
    MAXCLK=$(nvidia-smi --query-gpu=clocks.max.sm --format=csv,noheader,nounits 2>/dev/null | head -1)
    TARGET=$(( MAXCLK * 60 / 100 ))
    [ "$TARGET" -gt 0 ] || TARGET=1200
    if OUT=$(nvidia-smi --lock-gpu-clocks="$TARGET,$TARGET" 2>&1); then
        sleep 2
        GOT=$(nvidia-smi --query-gpu=clocks.sm,clocks.max.sm --format=csv,noheader 2>/dev/null)
        ok "锁频命令被接受（目标 ${TARGET} MHz）"
        echo "     当前 / 上限: $GOT"
        nvidia-smi --reset-gpu-clocks >/dev/null 2>&1
        sleep 1
        RC=$(nvidia-smi --query-gpu=clocks.max.sm --format=csv,noheader,nounits 2>/dev/null | head -1)
        if [ "$RC" = "$MAXCLK" ]; then
            ok "已复位（上限回到 ${MAXCLK} MHz）→ **GPU 锁频可用，可做成控件**"
        else
            warn "复位后上限为 $RC（期望 $MAXCLK），锁频可能被驱动锁住"
        fi
    else
        warn "锁频被拒绝：$(echo "$OUT" | head -1)"
        nvidia-smi --reset-gpu-clocks >/dev/null 2>&1 || true
    fi
else
    echo "     ⏭ 未找到 nvidia-smi，跳过"
fi

# ---------------------------------------------------------------- 汇总
echo
echo "════════════════════════════════════════════════"
printf "结果：通过 %d 项，失败 %d 项，提醒 %d 项\n" "$PASS" "$FAIL" "$WARN"
if [ "$FAIL" -eq 0 ]; then
    echo "所有功能均实际生效，且原值已恢复。"
    [ "$WARN" -gt 0 ] && echo "（上面有提醒项，属可选项，不影响功能）"
    exit 0
else
    echo "存在失败项，请把上面输出发回排查。"
    exit 1
fi
