#!/usr/bin/env bash
# 原版 bash 守护进程 vs Rust 守护进程对拍。
# 场景：
#   A 空闲小表：无流量，窗口内覆盖原版 60s 全表校对与原 ENOBUFS 兜底周期
#   B 客户端流量：3 个客户端每 0.1s 各发一个 UDP 包（持续 NEW/DESTROY 事件流）
#   C 大表校对：注入 2000 条 conntrack 条目后保持静默，测「全表 dump 兜底」的 CPU 成本
# 配置使用两边默认值（RESYNC/TTL=300 等）。CPU 按服务端 netns 全树统计
# （utime+stime+cutime+cstime，含已回收子进程与孤儿化的事件流水线）。
# 用法：bash /src/tests/perf-compare.sh [窗口秒数，默认 120]
set -euo pipefail

SERVER=perf-srv
CLIENT=perf-cli
WAN=wan-perf
PEER=peer-perf
IFB=ifb-perf
DURATION=${1:-120}
IPLIMIT_RS=${IPLIMIT_RS:-/usr/local/bin/iplimit}
IPLIMIT_BASH=${IPLIMIT_BASH:-/usr/local/bin/iplimit-bash}
LISTENER=""
TRAFFIC=""
INJECTED=0
NETNS_INODE=""
RESULTS=""

log() { printf '[对拍] %s\n' "$*"; }
die() { printf '[对拍] %s\n' "$*" >&2; exit 1; }

srv() { nsenter --net="/var/run/netns/$SERVER" "$@"; }
cli() { nsenter --net="/var/run/netns/$CLIENT" "$@"; }

reset_env() {
    ip netns delete "$SERVER" >/dev/null 2>&1 || true
    ip netns delete "$CLIENT" >/dev/null 2>&1 || true
}

setup_topo() {
    reset_env
    INJECTED=0
    ip netns add "$SERVER"
    ip netns add "$CLIENT"
    ip link add "$WAN" type veth peer name "$PEER"
    ip link set "$WAN" netns "$SERVER"
    ip link set "$PEER" netns "$CLIENT"
    srv ip addr add 203.0.113.10/24 dev "$WAN"
    srv ip -6 addr add 2001:db8:1::10/64 dev "$WAN"
    srv ip link set lo up
    srv ip link set "$WAN" up
    cli ip addr add 203.0.113.20/24 dev "$PEER"
    cli ip addr add 203.0.113.30/24 dev "$PEER"
    cli ip -6 addr add 2001:db8:1::20/64 dev "$PEER"
    cli ip link set lo up
    cli ip link set "$PEER" up
    srv iptables -t nat -A POSTROUTING -o "$WAN" -j MASQUERADE
    srv sh -c 'echo 1 > /proc/sys/net/ipv4/ip_forward'
    srv sh -c 'echo 1 > /proc/sys/net/ipv6/conf/all/forwarding'
    srv nc -u -l -p 9999 >/dev/null 2>&1 &
    LISTENER=$!
    sleep 0.3
    kill -0 "$LISTENER" 2>/dev/null || die "UDP 端点启动失败"
}

teardown_topo() {
    [[ -n "$TRAFFIC" ]] && { kill "$TRAFFIC" 2>/dev/null || true; wait "$TRAFFIC" 2>/dev/null || true; TRAFFIC=""; }
    [[ -n "$LISTENER" ]] && { kill "$LISTENER" 2>/dev/null || true; wait "$LISTENER" 2>/dev/null || true; LISTENER=""; }
    reset_env
}

# —— 服务端 netns 全树统计（事件流水线会孤儿化到 PID 1，不能按 ppid 统计） ——
init_netns_inode() {
    NETNS_INODE=$(srv readlink /proc/self/ns/net | sed 's/.*\[\(.*\)\]/\1/')
}

netns_pids() {
    local target=$NETNS_INODE link inode pid
    for link in /proc/[0-9]*/ns/net; do
        inode=$(readlink "$link" 2>/dev/null) || continue
        [[ "$inode" == "net:[$target]" ]] || continue
        pid=${link#/proc/}
        printf '%s\n' "${pid%/ns/net}"
    done
}

netns_cpu_seconds() {
    netns_pids | while read -r pid; do
        [[ -r "/proc/$pid/stat" ]] || continue
        awk '{ printf "%.3f\n", ($14 + $15 + $16 + $17) / 100 }' "/proc/$pid/stat"
    done | awk '{ s += $1 } END { printf "%.3f", s }'
}

netns_rss_kb() {
    local total=0 pid v
    while read -r pid; do
        v=$(awk '/VmRSS/ { print $2 }' "/proc/$pid/status" 2>/dev/null || echo 0)
        total=$((total + ${v:-0}))
    done < <(netns_pids)
    echo "$total"
}

netns_procs() { netns_pids | wc -w; }

start_traffic() {
    nsenter --net="/var/run/netns/$CLIENT" sh -c '
        while true; do
            for src in 203.0.113.20 203.0.113.30; do
                printf x | nc -u -w1 -s "$src" 203.0.113.10 9999 >/dev/null 2>&1
            done
            printf x | nc -6 -u -w1 -s 2001:db8:1::20 2001:db8:1::10 9999 >/dev/null 2>&1
            sleep 0.1
        done' &
    TRAFFIC=$!
}

stop_traffic() {
    [[ -n "$TRAFFIC" ]] && { kill "$TRAFFIC" 2>/dev/null || true; wait "$TRAFFIC" 2>/dev/null || true; }
    TRAFFIC=""
}

# 场景 C：注入 2000 条本机服务端 UDP 条目（dst 是本机 lo 地址，不会被判定为客户端），
# 表大而事件静默，窗口内测的是「全表 dump 兜底」本身的成本。
inject_table() {
    local seq_no
    for seq_no in $(seq 1 2000); do
        srv conntrack -I -p udp --timeout 600 \
            -s 127.0.0.11 -d 127.0.0.11 --sport "$((40000 + seq_no))" --dport 53 \
            >/dev/null 2>&1
    done
    local present
    present=$(srv conntrack -L -p udp 2>/dev/null | grep -c '^udp' || true)
    log "已注入 conntrack 条目：$present 条"
    INJECTED=1
}

window_cpu() {
    # $1 = 描述，结果秒数写入 WINDOW_CPU
    local before=$(netns_cpu_seconds)
    sleep "$DURATION"
    local after=$(netns_cpu_seconds)
    WINDOW_CPU=$(awk -v a="$before" -v b="$after" 'BEGIN { printf "%.3f", b - a }')
    log "$1：${WINDOW_CPU}s CPU / ${DURATION}s 窗口"
}

measure_variant() {
    local name=$1 bin=$2 state_dir=$3
    local conf="/tmp/perf-$name.conf"
    cat > "$conf" <<EOF
RATE_MBIT="2"
INTERFACE="$WAN"
IFB_NAME="$IFB"
POLL_INTERVAL="2"
RESYNC_INTERVAL="300"
CLIENT_TTL="300"
UNSHAPED_RATE_MBIT="100000"
EOF
    rm -rf "$state_dir"

    setup_topo
    local t0=$(date +%s.%N)
    nsenter --net="/var/run/netns/$SERVER" env \
        IPLIMIT_CONFIG_FILE="$conf" IPLIMIT_STATE_DIR="$state_dir" \
        "$bin" daemon-internal &
    local daemon=$!
    local ok=0
    for _ in $(seq 1 400); do
        kill -0 "$daemon" 2>/dev/null || die "$name 守护进程启动失败"
        if srv tc qdisc show dev "$WAN" 2>/dev/null | grep -q 'qdisc htb 1:'; then
            ok=1
            break
        fi
        sleep 0.05
    done
    local t1=$(date +%s.%N)
    [[ $ok -eq 1 ]] || die "$name 启动就绪超时"
    local startup=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }')
    init_netns_inode
    sleep 2

    # 场景 A：空闲小表
    window_cpu "$name A 空闲小表"
    local idle_a=$WINDOW_CPU

    # 场景 B：持续客户端流量（事件路径）
    start_traffic
    # 原版 clients.tsv 最快 5 秒批量落盘，多等一会再核对发现结果
    sleep 12
    local seen
    seen=$(grep -c '^[0-9]' "$state_dir/clients.tsv" 2>/dev/null || true)
    log "$name B 事件路径发现客户端：${seen:-0} 个（应为 3）"
    window_cpu "$name B 客户端流量"
    local busy_b=$WINDOW_CPU
    stop_traffic

    # 场景 C：大表静默（全表校对路径）
    inject_table
    sleep 5
    window_cpu "$name C 大表 2000 条"
    local busy_c=$WINDOW_CPU

    # 场景 D：大表 + 长窗口静默，覆盖两边的周期性全表 dump
    # （原版 effective resync = min(RESYNC, TTL/3)=100s；Rust 对账 = 300s）
    local long_window=${LONG_WINDOW:-620}
    local before_d=$(netns_cpu_seconds)
    sleep "$long_window"
    local after_d=$(netns_cpu_seconds)
    local idle_d=$(awk -v a="$before_d" -v b="$after_d" 'BEGIN { printf "%.3f", b - a }')
    log "$name D 大表长窗口：${idle_d}s CPU / ${long_window}s"

    local rss=$(netns_rss_kb)
    local procs=$(netns_procs)

    kill -TERM "$daemon" 2>/dev/null || true
    wait "$daemon" 2>/dev/null || true
    for _ in $(seq 1 40); do kill -0 "$daemon" 2>/dev/null || break; sleep 0.1; done

    local span=$DURATION
    RESULTS+=$(awk -v n="$name" -v s="$startup" -v a="$idle_a" -v b="$busy_b" -v c="$busy_c" -v e="$idle_d" -v r="$rss" -v p="$procs" -v d="$span" -v L="$long_window" \
        'BEGIN { printf "%-5s | 启动 %ss | A空闲 %.3f%% | B流量 %.3f%% | C大表 %.3f%% | D大表长窗 %.4f%% | RSS %dKB | 常驻进程 %d |\n", n, s, a / d * 100, b / d * 100, c / d * 100, e / L * 100, r, p }')$'\n'
    teardown_topo
}

[[ $(id -u) -eq 0 ]] || die "需要 root"
[[ -x "$IPLIMIT_RS" ]] || die "缺少 $IPLIMIT_RS"
[[ -x "$IPLIMIT_BASH" ]] || die "缺少 $IPLIMIT_BASH"
command -v systemd-notify >/dev/null 2>&1 || { printf '#!/bin/sh\nexit 0\n' > /usr/local/bin/systemd-notify; chmod +x /usr/local/bin/systemd-notify; }
command -v systemctl >/dev/null 2>&1 || { printf '#!/bin/sh\nexit 0\n' > /usr/local/bin/systemctl; chmod +x /usr/local/bin/systemctl; }

trap 'stop_traffic 2>/dev/null; teardown_topo 2>/dev/null' EXIT

log "窗口=${DURATION}s/场景，配置为两边默认（RESYNC/TTL=300）"
measure_variant "原版" "$IPLIMIT_BASH" /tmp/perf-state-orig
measure_variant "Rust" "$IPLIMIT_RS" /tmp/perf-state-rs
printf '\n==== 汇总（CPU%% 为窗口内 netns 全树平均占用率；A/B/C 窗口 %ss，D 窗口 %ss） ====\n| 实现 | 启动 | A 空闲小表 | B 客户端流量 | C 大表 2000 条 | D 大表长窗静默 | RSS | 常驻进程 |\n|---|---|---|---|---|---|---|---|\n%s' "$DURATION" "${LONG_WINDOW:-620}" "$RESULTS"
