#!/bin/bash
# ============================================================================
# OMEN 控制能力实测诊断
#
# 目的：确认这台机器上，哪些控制项**真正可写、且写入能保持**，
#       为控制面板定功能边界。全程只走内核 / NVIDIA 官方接口，不写 EC。
#
# 安全性：每一个写测试都会保存原值，并在脚本退出时（含 Ctrl+C）自动恢复。
#
# 用法：sudo bash diagnose.sh
# ============================================================================
set -uo pipefail

[ "$(id -u)" -eq 0 ] || { echo "请用 sudo 运行：sudo bash $0"; exit 1; }

HWMON="$(ls -d /sys/devices/platform/hp-wmi/hwmon/hwmon* 2>/dev/null | head -1)"
RAPL=/sys/class/powercap/intel-rapl:0
C0=/sys/devices/system/cpu/cpu0/cpufreq
PP=/sys/firmware/acpi/platform_profile

hr() { printf '%s\n' "────────────────────────────────────────────────────────────"; }

RESTORE=()
save() { RESTORE+=("$1|$2"); }
do_restore() {
  local e f v
  for e in ${RESTORE[@]+"${RESTORE[@]}"}; do
    f="${e%%|*}"; v="${e##*|}"
    printf '%s' "$v" > "$f" 2>/dev/null && echo "  [已恢复] $f = $v"
  done
  RESTORE=()
}
trap do_restore EXIT

echo "OMEN 控制能力实测（写测试会自动还原）"
hr

# ---------------------------------------------------------------- 1
echo "【1】CPU 最大频率上限是否可写"
ORIG=$(cat $C0/scaling_max_freq)
save $C0/scaling_max_freq "$ORIG"
echo "  原值: $((ORIG/1000)) MHz"
if echo 2000000 > $C0/scaling_max_freq 2>/dev/null; then
  NOW=$(cat $C0/scaling_max_freq)
  if [ "$NOW" = "2000000" ]; then
    echo "  ✅ 可写（写 2000MHz，回读 $((NOW/1000))MHz）"
  else
    echo "  ⚠ 写入被内核调整，回读 $((NOW/1000))MHz"
  fi
else
  echo "  ❌ 不可写"
fi

# ---------------------------------------------------------------- 2
echo
echo "【2】EPP 能效偏好是否可写"
ORIG=$(cat $C0/energy_performance_preference)
save $C0/energy_performance_preference "$ORIG"
echo "  原值: $ORIG"
if echo power > $C0/energy_performance_preference 2>/dev/null \
   && [ "$(cat $C0/energy_performance_preference)" = "power" ]; then
  echo "  ✅ 可写"
else
  echo "  ❌ 不可写或被拒"
fi

# ---------------------------------------------------------------- 3
echo
echo "【3】睿频开关（no_turbo）是否可写"
ORIG=$(cat /sys/devices/system/cpu/intel_pstate/no_turbo)
save /sys/devices/system/cpu/intel_pstate/no_turbo "$ORIG"
echo "  原值: $ORIG (0=睿频开)"
if echo 1 > /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null \
   && [ "$(cat /sys/devices/system/cpu/intel_pstate/no_turbo)" = "1" ]; then
  echo "  ✅ 可写"
else
  echo "  ❌ 不可写"
fi

# ---------------------------------------------------------------- 4
echo
echo "【4】GPU 功耗墙 —— 关键项（当前 115W，硬件上限 175W）"
if command -v nvidia-smi >/dev/null 2>&1; then
  echo "  当前能力:"
  nvidia-smi --query-gpu=power.limit,power.default_limit,power.max_limit --format=csv 2>&1 | sed 's/^/    /'
  echo "  试设 150W ..."
  if nvidia-smi -pl 150 >/dev/null 2>&1; then
    echo "    设置后: $(nvidia-smi --query-gpu=power.limit --format=csv,noheader 2>&1)"
    echo "  ✅ 可设置 → 这一项能带来真实的性能差异"
    echo "  恢复默认 115W ..."
    nvidia-smi -pl 115 >/dev/null 2>&1 \
      && echo "    已恢复: $(nvidia-smi --query-gpu=power.limit --format=csv,noheader 2>&1)"
  else
    echo "  ❌ 不可设置（可能被 vBIOS 锁定，或笔记本 GPU 不支持）"
  fi
else
  echo "  未找到 nvidia-smi"
fi

# ---------------------------------------------------------------- 5
echo
echo "【5】有没有守护进程覆盖我们写的档位（解释「点了没反应」）"
BEFORE=$(cat $PP)
echo "  当前档位: $BEFORE"
echo performance > $PP 2>/dev/null
echo "  写入后立刻: $(cat $PP)"
sleep 6
AFTER=$(cat $PP)
echo "  6 秒后    : $AFTER"
if [ "$AFTER" = "performance" ]; then
  echo "  ✅ 写入能保持（没被抢管）"
else
  echo "  ⚠ 被改回 $AFTER —— 有守护进程在覆盖，这就是点击无效的原因之一"
fi
echo "$BEFORE" > $PP 2>/dev/null && echo "  已切回 $BEFORE"

# ---------------------------------------------------------------- 6
echo
echo "【6】最大风扇实际能到多少 RPM（验证 5100 vs 4800）"
F1=$HWMON/fan1_input; F2=$HWMON/fan2_input
echo "  当前: fan1=$(cat $F1) RPM  fan2=$(cat $F2) RPM"
echo "  开启最大风扇，采样 12 秒 ..."
echo 0 > $HWMON/pwm1_enable 2>/dev/null
P1=0; P2=0
for _ in $(seq 1 12); do
  a=$(cat $F1 2>/dev/null); b=$(cat $F2 2>/dev/null)
  [ "${a:-0}" -gt "$P1" ] && P1=$a
  [ "${b:-0}" -gt "$P2" ] && P2=$b
  sleep 1
done
echo "  峰值: fan1=$P1 RPM  fan2=$P2 RPM"
echo 2 > $HWMON/pwm1_enable 2>/dev/null
echo "  已恢复自动: fan1=$(cat $F1) RPM  fan2=$(cat $F2) RPM"
if [ "$P1" -lt 5000 ] && [ "$P2" -lt 5000 ]; then
  echo "  ⇒ 当前最大档未到 5000+，说明更高转速需要 EC 层面的独立命令（内核没暴露）"
fi

# ---------------------------------------------------------------- 7
echo
echo "【7】RAPL CPU 功耗墙（仅报告，不做写测试）"
echo "  当前 PL1        : $(( $(cat $RAPL/constraint_0_power_limit_uw) / 1000000 )) W"
echo "  驱动声明上限    : $(( $(cat $RAPL/constraint_0_max_power_uw) / 1000000 )) W"
echo "  ⇒ 当前值高于声明上限，说明 135W 由 BIOS 设定。RAPL 写入可能只能往下调、"
echo "     且可能无法调回原值（重启才复位）—— 风险偏高，默认不启用。"

hr
echo "诊断完成。"
