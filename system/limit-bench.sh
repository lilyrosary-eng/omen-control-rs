#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════
# limit-bench.sh —— 量化 CPU/GPU 限制手段的**实际效果**
#
# 背景：手段早就有了（max_perf_pct / no_turbo / cpu_cap / PL1 / -lgc），
#       但**从来没量化过**——降了几度？代价多少性能？这个脚本回答它。
#
# 用法
#   sudo bash limit-bench.sh cpu            # CPU 四种设置的效果矩阵（约 6 分钟）
#   sudo bash limit-bench.sh gpu            # GPU -lgc 的效果（需要 glxgears）
#   sudo bash limit-bench.sh cpu --quick    # 快速版（约 3 分钟，样本少一半）
#
# 安全
#   · 每个场景开始前先把所有限制**还原成原始值**，场景之间互不叠加
#   · 任何退出路径（含 Ctrl-C / 关终端）都会还原
#   · 只改这几项：max_perf_pct、no_turbo、scaling_max_freq、PL1
#
# ⚠ 想验证「Ctrl-C 会不会还原」时，**别用 `... &` 把脚本丢到后台**：
#   POSIX 规定非交互 shell 在后台启动时 SIGINT/SIGQUIT 被设为 SIG_IGN，
#   而「进入时就已被忽略的信号无法被 trap 捕获」⇒ 你会误判成「还原失败」。
#   验证请用 kill -TERM / kill -HUP（这两种不会被忽略）。已实测确认。
# ═══════════════════════════════════════════════════════════════════════════
set -uo pipefail

MODE="${1:-}"
QUICK=0
[ "${2:-}" = "--quick" ] && QUICK=1

CORES=${CORES:-8}
if [ "$QUICK" = 1 ]; then DEF_W=20; DEF_N=10; DEF_C=25; else DEF_W=35; DEF_N=18; DEF_C=40; fi
# 三段时长都可用环境变量覆盖（替身测试 / 快速试跑都靠它）
WARM=${WARM:-$DEF_W}; NSAMP=${NSAMP:-$DEF_N}; COOL=${COOL:-$DEF_C}

OUT=/tmp/limit-bench.tsv
TMPF=/tmp/limit-bench-tmp.txt
PY="${PYTHON:-python3}"   # 可覆盖：PYTHON=/path/to/python3 bash 本脚本
[ -x "$PY" ] || PY=$(command -v python3)

# ⚠ 全部可被环境变量覆盖 —— 这不是为了灵活，是为了**能用替身跑通全流程**
#   （真机验证要 root + 加热 6 分钟；替身能在几秒内把逻辑走一遍）
PSTATE=${PSTATE:-/sys/devices/system/cpu/intel_pstate}
POLICY=${POLICY:-/sys/devices/system/cpu/cpufreq/policy0}
RAPL_DIR=${RAPL_DIR:-/sys/class/powercap/intel-rapl:0}
PL1=${PL1:-$RAPL_DIR/constraint_0_power_limit_uw}
# short_term（PL2）。**必须一起限**，否则 PL1 的 56 秒窗口在 20 秒场景里咬不住（见 apply_pl1）
PL2=${PL2:-$RAPL_DIR/constraint_1_power_limit_uw}
HWM=${HWM:-/sys/class/hwmon/hwmon7}
OMEN=${OMEN:-/sys/devices/platform/omen-wmi}

R=$'\033[31m'; G=$'\033[32m'; Y=$'\033[33m'; B=$'\033[1m'; N=$'\033[0m'
hdr()  { printf '\n%s%s%s\n' "$B" "$*" "$N"; }
ok()   { printf '  %s✓%s %s\n' "$G" "$N" "$*"; }
warn() { printf '  %s!%s %s\n' "$Y" "$N" "$*"; }
bad()  { printf '  %s✗%s %s\n' "$R" "$N" "$*"; }
info() { printf '    %s\n' "$*"; }
dim()  { printf '    \033[2m%s\033[0m\n' "$*"; }

# ── 原值记录 / 还原 ──────────────────────────────────────────────────────────
ORIG_PCT=""; ORIG_TURBO=""; ORIG_CAP=""; ORIG_PL1=""; ORIG_PL2=""
LOAD_PIDS=()

save_orig() {
    ORIG_PCT=$(cat "$PSTATE/max_perf_pct" 2>/dev/null)
    ORIG_TURBO=$(cat "$PSTATE/no_turbo" 2>/dev/null)
    ORIG_CAP=$(cat "$POLICY/scaling_max_freq" 2>/dev/null)
    ORIG_PL1=$(cat "$PL1" 2>/dev/null)
    ORIG_PL2=$(cat "$PL2" 2>/dev/null)
}

stop_load() {
    local p
    for p in "${LOAD_PIDS[@]:-}"; do
        [ -n "$p" ] && kill "$p" 2>/dev/null
    done
    # ⚠ 只 wait 具体 PID，**不能用裸 wait** ——
    #   无参 wait 会把别的后台任务一起等上，可能永久阻塞（2026-10-10 在 fan-calibrate.sh 踩过）。
    for p in "${LOAD_PIDS[@]:-}"; do
        [ -n "$p" ] && wait "$p" 2>/dev/null
    done
    LOAD_PIDS=()
}

# DIRTY=1 表示「我们设的限制正生效，需要还原」；restore_all 会置 0。
# 应用任何限制之后必须 DIRTY=1，这样 _on_exit 才知道要不要收尾。
DIRTY=0
restore_all() {
    stop_load
    [ -n "$ORIG_PCT" ]   && printf '%s' "$ORIG_PCT"   > "$PSTATE/max_perf_pct" 2>/dev/null
    [ -n "$ORIG_TURBO" ] && printf '%s' "$ORIG_TURBO" > "$PSTATE/no_turbo"     2>/dev/null
    [ -n "$ORIG_CAP" ]   && printf '%s' "$ORIG_CAP"   > "$POLICY/scaling_max_freq" 2>/dev/null
    [ -n "$ORIG_PL1" ]   && printf '%s' "$ORIG_PL1"   > "$PL1"                 2>/dev/null
    [ -n "$ORIG_PL2" ]   && printf '%s' "$ORIG_PL2"   > "$PL2"                 2>/dev/null
    DIRTY=0
}

_on_exit() {
    trap - EXIT INT TERM HUP
    printf '\n'
    if [ "$DIRTY" = 1 ]; then
        warn "收尾：还原所有限制…"
        restore_all
        ok "已还原：max_perf_pct=$ORIG_PCT no_turbo=$ORIG_TURBO cap=$ORIG_CAP PL1=$(uw2w "$ORIG_PL1") W"
    else
        dim "无待还原的限制"
    fi
    exit 0
}

need_root() {
    [ "$(id -u)" = 0 ] || { bad "需要 root：sudo bash $0 $MODE"; exit 1; }
}

# ── 读数 ─────────────────────────────────────────────────────────────────────
uw2w() { "$PY" -c "print(round(${1:-0}/1e6,1))" 2>/dev/null || echo 0; }

pkg_temp() {   # Package 温度（**不用各核最高** —— 那是毛刺指标，实测可差 30°C）
    local f l
    for f in "$HWM"/temp*_input; do
        l=$(cat "${f%_input}_label" 2>/dev/null)
        case "$l" in *Package*) echo $(( $(cat "$f")/1000 )); return ;; esac
    done
    # 回退：各核平均
    { for f in "$HWM"/temp*_input; do [ -r "$f" ] && echo $(( $(cat "$f")/1000 )); done; } \
        | awk '{s+=$1;n++} END{if(n)printf "%.0f",s/n; else print 0}'
}

pkg_watt() {   # 用 RAPL energy 差值算平均功耗（1 秒窗口）
    local e1 e2
    e1=$(cat "$RAPL_DIR/energy_uj" 2>/dev/null) || { echo 0; return; }
    sleep 1
    e2=$(cat "$RAPL_DIR/energy_uj" 2>/dev/null) || { echo 0; return; }
    "$PY" -c "print(round((${e2}-${e1})/1e6,1))" 2>/dev/null || echo 0
}

avg_mhz() {
    awk '/cpu MHz/{s+=$4;n++} END{if(n)printf "%.0f",s/n; else print 0}' /proc/cpuinfo
}

max_mhz() {
    awk '/cpu MHz/{if($4>m)m=$4} END{printf "%.0f",m}' /proc/cpuinfo
}

fan_rpm() { cat "$OMEN/fan_rpm" 2>/dev/null | tr '\n' '/' | sed 's|/$||'; }

# ── 负载 ─────────────────────────────────────────────────────────────────────
start_load() {
    local n=$1 i
    for i in $(seq 1 "$n"); do
        "$PY" -c '
import time
t = time.time()
x = 0
while time.time() - t < 300:
    x = (x * 1103515245 + 12345) & 0x7fffffff
' &
        LOAD_PIDS+=($!)
    done
}

# ── 一个场景的完整测量 ───────────────────────────────────────────────────────
measure() {   # $1=标签
    local tag=$1 i
    start_load "$CORES"
    printf '  %-22s 升温 %ss' "$tag" "$WARM"
    sleep "$WARM"
    : > "$TMPF"
    for i in $(seq 1 "$NSAMP"); do
        printf '%s\t%s\t%s\n' "$(pkg_temp)" "$(pkg_watt)" "$(avg_mhz)" >> "$TMPF"
        printf '.'
    done
    stop_load
    printf ' 采集完，冷却 %ss\n' "$COOL"
    awk -v tag="$tag" -F'\t' \
        '{t+=$1; w+=$2; m+=$3; n++; if($1>tm)tm=$1} END{
           if(n) printf "%s\t%.1f\t%.1f\t%.0f\t%d\t%d\n", tag, t/n, w/n, m/n, tm, n
         }' "$TMPF" >> "$OUT"
    sleep "$COOL"
}

# ── 各项限制的应用 ───────────────────────────────────────────────────────────
# 每项都写后读回校验：内核/固件可能拒绝（比如超出可写范围），静默失败会让结论错
apply_pct() {
    printf '%s' "$1" > "$PSTATE/max_perf_pct" 2>/dev/null
    local rb; rb=$(cat "$PSTATE/max_perf_pct" 2>/dev/null)
    DIRTY=1
    [ "$rb" = "$1" ] || warn "max_perf_pct 写 $1 读回 $rb（未生效）"
}
apply_turbo() {
    printf '%s' "$1" > "$PSTATE/no_turbo" 2>/dev/null
    local rb; rb=$(cat "$PSTATE/no_turbo" 2>/dev/null)
    DIRTY=1
    [ "$rb" = "$1" ] || warn "no_turbo 写 $1 读回 $rb（未生效）"
}
# ⚠⚠ 2026-10-11 修掉的真 bug：这里原来**只写 constraint_0（PL1）**。
#   PL1 是**约 56 秒窗口的平均值**，而本脚本每场景只升温 20 秒 ——
#   窗口还没走完，PL1 根本咬不住；同时 short_term（PL2）仍是 135 W，
#   于是"PL1=45W"那一行测出来功耗 59.1 W、峰值 99°C，**看起来像"限功耗没用"**。
#   实测对照：PL1+PL2 一起压时 30 W 能在 2 秒内把温度从 64°C 拉到 55°C（见 README）。
#   ⇒ 必须连 PL2 一起写，这一场景才测的是"限功耗"而不是"什么都没限"。
apply_pl1() {
    printf '%s' "$1" > "$PL1" 2>/dev/null
    [ -w "$PL2" ] && printf '%s' "$1" > "$PL2" 2>/dev/null
    local rb rb2; rb=$(cat "$PL1" 2>/dev/null); rb2=$(cat "$PL2" 2>/dev/null)
    DIRTY=1
    [ "$rb" = "$1" ] || warn "PL1 写 $1 读回 $rb（未生效）"
    [ "$rb2" = "$1" ] || warn "PL2 写 $1 读回 $rb2（未生效 —— 只压 PL1 在 20s 窗口里几乎没有效果）"
}

readback() {
    printf '    max_perf_pct=%-4s no_turbo=%-2s cap=%-9s PL1=%s W · PL2=%s W\n' \
        "$(cat "$PSTATE/max_perf_pct" 2>/dev/null)" \
        "$(cat "$PSTATE/no_turbo" 2>/dev/null)" \
        "$(cat "$POLICY/scaling_max_freq" 2>/dev/null)" \
        "$(uw2w "$(cat "$PL1" 2>/dev/null)")" \
        "$(uw2w "$(cat "$PL2" 2>/dev/null)")"
}

# ── CPU 场景矩阵 ─────────────────────────────────────────────────────────────
do_cpu() {
    need_root
    hdr "CPU 限制效果矩阵"
    info "负载 $CORES 核 · 升温 ${WARM}s · 采样 ${NSAMP} 次 · 冷却 ${COOL}s"
    info "指标：Package 温度 / RAPL 封装功耗 / 平均频率（均取采样窗平均）"
    info "数据：$OUT"

    hdr "原始设置"
    readback

    if [ -f "$OMEN/fan_speed" ]; then
        local fs
        fs=$(cat "$OMEN/fan_speed" 2>/dev/null | tr '\n' ' ')
        case "$fs" in
            *set=auto*) ok  "风扇当前是自动（BIOS 曲线）—— 结果可反映真实散热" ;;
            *)          warn "风扇当前被手动控制（$fs）⇒ 温度结果会被这个固定转速影响" ;;
        esac
    fi

    printf '场景\t温度°C\t功耗W\t平均MHz\t峰值温度\tn\n' > "$OUT"
    hdr "开始测量（别动鼠标键盘，避免额外负载）"

    # ① 基线（原设置）
    restore_all; save_orig
    measure "① 基线（当前设置）"

    # ② max_perf_pct = 50
    restore_all
    apply_pct 50
    measure "② max_perf_pct=50"

    # ③ 关睿频
    restore_all
    apply_turbo 1
    measure "③ no_turbo=1"

    # ④ PL1 = 45 W
    restore_all
    apply_pl1 45000000
    measure "④ PL1=45W"

    restore_all

    hdr "结果"
    column -t -s$'\t' "$OUT" | sed 's/^/  /'

    hdr "对比（相对基线）"
    awk -F'\t' 'NR==2{b_t=$2; b_w=$3; b_m=$4; next}
        NR>2{
            printf "  %-22s 温度 %+5.1f°C   功耗 %+6.1f W   频率 %+5.0f MHz\n",
                $1, $2-b_t, $3-b_w, $4-b_m
        }' "$OUT"

    hdr "读法"
    cat <<'EOF'
    · 温度下降 = 这套限制的散热收益；功耗下降 = 供电收益
    · 频率下降 = 直接代价（多核吞吐大致与频率成正比）
    · **注意**：风扇是自动时，降温会让 BIOS 曲线也降速 —— 这是"省了转速"，
      不只是"降了温度"。真正要看的是「功耗」和「频率」两列。
EOF
}

# ── GPU 场景 ─────────────────────────────────────────────────────────────────
do_gpu() {
    need_root
    hdr "GPU 时钟锁（-lgc）的效果"

    if ! command -v nvidia-smi >/dev/null 2>&1; then
        bad "没有 nvidia-smi"; exit 1
    fi
    if ! command -v glxgears >/dev/null 2>&1; then
        warn "没有 glxgears —— 无法自己造 GPU 负载"
        info "装一个：sudo apt install mesa-utils"
        info "或者用你平时的 GPU 负载（游戏/渲染），手动对比开/关 -lgc 时的温度与频率"
        exit 1
    fi

    local minmax cur
    minmax=$(nvidia-smi --query-gpu=clocks.min_gr,clocks.max_gr --format=csv,noheader 2>/dev/null)
    info "GPU 支持的频率范围: $minmax"
    cur=$(nvidia-smi --query-gpu=clocks.gr,clocks.mem,temperature.gpu,power.draw --format=csv,noheader 2>/dev/null)
    info "当前: $cur"

    warn "glxgears 是 OpenGL 负载，功耗通常只有几十瓦 —— 可能看不出明显差异"
    info "若结果差异不明显，请用你真实的 GPU 负载（游戏/推理）手动复测"
    info "开跑：先空载基线，再施加 glxgears 负载，再对比 -lgc"

    local lo hi
    lo=$(nvidia-smi --query-supported-clocks=gpu --format=csv,noheader 2>/dev/null | head -1 | awk '{print $1}')
    hi=$(nvidia-smi --query-supported-clocks=gpu --format=csv,noheader 2>/dev/null | tail -1 | awk '{print $1}')
    info "计划锁定范围: ${lo:-500}–1500 MHz（示例）；实际以本机支持为准"

    printf '场景\tGPU温度\tGPU功耗W\tSM频率MHz\n' > "$OUT"

    run_gpu_case() {   # $1=标签 $2=秒 $3=锁频（空=不锁）
        local tag=$1 secs=$2 lock=$3 i
        export DISPLAY="${DISPLAY:-:0}"
        if [ -n "$lock" ]; then
            nvidia-smi -lgc "$lock" >/dev/null 2>&1 || warn "  -lgc $lock 失败"
        else
            nvidia-smi -rgc >/dev/null 2>&1
        fi
        vblank_mode=0 glxgears -fullscreen >/dev/null 2>&1 &
        GPU_LOAD_PID=$!
        printf '  %-24s 负载 %ss' "$tag" "$secs"
        sleep "$((secs / 2))"
        : > "$TMPF"
        for i in $(seq 1 6); do
            nvidia-smi --query-gpu=temperature.gpu,power.draw,clocks.gr \
                --format=csv,noheader,nounits 2>/dev/null | tr -d ' ' >> "$TMPF"
            printf '.'
            sleep 2
        done
        kill "$GPU_LOAD_PID" 2>/dev/null
        GPU_LOAD_PID=""     # 别用裸 wait
        awk -v tag="$tag" -F',' '{t+=$1;w+=$2;c+=$3;n++} END{
            if(n) printf "%s\t%.0f\t%.1f\t%.0f\n", tag, t/n, w/n, c/n
        }' "$TMPF" >> "$OUT"
        printf '\n'
        nvidia-smi -rgc >/dev/null 2>&1
    }

    run_gpu_case "① 不锁频"        30 ""
    run_gpu_case "② 锁定 $lo–1500"  30 "$lo,1500"
    run_gpu_case "③ 锁定 $lo–800"   30 "$lo,800"

    hdr "结果"
    column -t -s$'\t' "$OUT" | sed 's/^/  /'
    hdr "收尾"
    nvidia-smi -rgc >/dev/null 2>&1 && ok "已解锁 GPU 频率（-rgc）"
    info "数据：$OUT"
}

# ── main ─────────────────────────────────────────────────────────────────────
case "$MODE" in
    cpu)
        save_orig
        trap '_on_exit' EXIT INT TERM HUP
        do_cpu
        ;;
    gpu)
        save_orig
        trap '_on_exit' EXIT INT TERM HUP
        do_gpu
        ;;
    *)
        cat <<EOF
用法：sudo bash $0 {cpu|gpu} [--quick]

  cpu          测 CPU 限制的效果矩阵（基线 / max_perf_pct=50 / no_turbo / PL1=45W）
               默认约 6 分钟；加 --quick 约 3 分钟
  gpu          测 GPU 时钟锁 -lgc 的效果（需要 glxgears 造负载）

可选环境变量：
  CORES=8      负载核数（默认 8）

数据输出：/tmp/limit-bench.tsv（agent 可直接读）
它只改这些并保证还原：max_perf_pct、no_turbo、scaling_max_freq、PL1
EOF
        ;;
esac
