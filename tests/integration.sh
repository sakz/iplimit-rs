#!/usr/bin/env bash

set -Eeuo pipefail

# 在拥有 NET_ADMIN 权限的 Linux 环境（或特权容器）中以 root 运行。
# 用法：IPLIMIT_BIN=/usr/local/bin/iplimit ./tests/integration.sh
#
# 测试在独立的服务端和客户端网络命名空间中运行，不会改动宿主机的
# 队列规则或 conntrack 表。

IPLIMIT_BIN=${IPLIMIT_BIN:-/usr/local/bin/iplimit}
SERVER="iplimit-server"
CLIENT="iplimit-client"
WAN="wan-iptest"
PEER="peer-iptest"
IFB="ifb-iptest"
STATE_DIR="/tmp/iplimit-it-state"
CONFIG_FILE="/tmp/iplimit-it.conf"
LOG_FILE="/tmp/iplimit-it-daemon.log"
PID_FILE="/tmp/iplimit-it-daemon.pid"
RUNNER_SCRIPT="/tmp/iplimit-it-runner.sh"
DAEMON_PID=""
LISTENER4=""
LISTENER6=""
ADDED_IPV4_NAT=0
ADDED_IPV6_CT=0

log() {
    printf '[集成测试] %s\n' "$*"
}

fail() {
    printf 'not ok - %s\n' "$*" >&2
    if [[ -f "$LOG_FILE" ]]; then
        printf '%s\n' '--- daemon log ---' >&2
        tail -n 80 "$LOG_FILE" >&2 || true
    fi
    exit 1
}

pass() {
    printf 'ok - %s\n' "$*"
}

# ip netns exec 会给子进程设置 PR_SET_PDEATHSIG（父进程退出即 SIGKILL），
# 信号转发包装无法存活，统一用 nsenter 进入命名空间。
srv() {
    nsenter --net="/var/run/netns/$SERVER" "$@"
}

cli() {
    nsenter --net="/var/run/netns/$CLIENT" "$@"
}

run_iplimit_srv() {
    nsenter --net="/var/run/netns/$SERVER" env \
        IPLIMIT_CONFIG_FILE="$CONFIG_FILE" \
        IPLIMIT_STATE_DIR="$STATE_DIR" \
        "$IPLIMIT_BIN" "$@"
}

cleanup() {
    set +e
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null
        wait "$DAEMON_PID" 2>/dev/null
    fi
    [[ -n "$LISTENER4" ]] && kill "$LISTENER4" 2>/dev/null
    [[ -n "$LISTENER6" ]] && kill "$LISTENER6" 2>/dev/null
    if [[ -f "$PID_FILE" ]]; then
        kill -TERM "$(cat "$PID_FILE")" 2>/dev/null
        sleep 0.3
    fi
    run_iplimit_srv cleanup-internal >/dev/null 2>&1
    if [[ "$ADDED_IPV4_NAT" == "1" ]]; then
        srv iptables -t nat -D POSTROUTING -o "$WAN" -j MASQUERADE >/dev/null 2>&1
    fi
    if [[ "$ADDED_IPV6_CT" == "1" ]]; then
        srv ip6tables -D INPUT -m conntrack --ctstate NEW,ESTABLISHED -j ACCEPT \
            >/dev/null 2>&1
    fi
    ip netns delete "$CLIENT" >/dev/null 2>&1
    ip netns delete "$SERVER" >/dev/null 2>&1
    ip link delete "$WAN" >/dev/null 2>&1
    rm -rf "$STATE_DIR" "$CONFIG_FILE" "$LOG_FILE" "$PID_FILE" "$RUNNER_SCRIPT" /tmp/iplimit-foreign.log
}

trap cleanup EXIT

wait_for() {
    local description=$1
    shift
    local attempts=200
    if [[ $1 == --attempts ]]; then
        attempts=$2
        shift 2
    fi
    local on_timeout=""
    if [[ $1 == --on-timeout ]]; then
        on_timeout=$2
        shift 2
    fi
    local attempt
    for attempt in $(seq 1 "$attempts"); do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        if [[ -n "$DAEMON_PID" ]] && ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            fail "守护进程提前退出（等待：$description）"
        fi
        sleep 0.1
    done
    if [[ -n "$on_timeout" ]]; then
        eval "$on_timeout" || true
    fi
    fail "等待超时：$description"
}

clients_have() {
    grep -Fq "$1" "$STATE_DIR/clients.tsv"
}

clients_missing() {
    ! grep -Fq "$1" "$STATE_DIR/clients.tsv"
}

root_htb_present() {
    srv tc qdisc show dev "$WAN" | grep -q 'qdisc htb 1: root'
}

ifb_present() {
    srv ip link show dev "$IFB" >/dev/null 2>&1
}

class_packets() {
    local device=$1
    local classid=$2
    srv tc -s class show dev "$device" classid "$classid" 2>/dev/null \
        | awk '/ Sent / { print $4; exit }'
}

send_udp4() {
    local source=$1
    printf x | cli nc -u -w1 -s "$source" 203.0.113.10 9999 >/dev/null 2>&1 || true
}

send_udp6() {
    local source=$1
    printf x | cli nc -6 -u -w1 -s "$source" 2001:db8:1::10 9999 >/dev/null 2>&1 || true
}

write_config() {
    local interface=$1
    cat >"$CONFIG_FILE" <<EOF
RATE_MBIT="2"
INTERFACE="$interface"
IFB_NAME="$IFB"
POLL_INTERVAL="1"
CLIENT_TTL="6"
UNSHAPED_RATE_MBIT="100000"
EXCLUDE_IPS="203.0.113.99"
EOF
    chmod 600 "$CONFIG_FILE"
}

start_daemon() {
    : >"$LOG_FILE"
    rm -f "$PID_FILE"
    nsenter --net="/var/run/netns/$SERVER" sh "$RUNNER_SCRIPT" \
        >>"$LOG_FILE" 2>&1 &
    DAEMON_PID=$!
}

stop_daemon() {
    kill -TERM "$DAEMON_PID"
    local status=0
    wait "$DAEMON_PID" || status=$?
    if [[ $status -ne 0 ]]; then
        fail "守护进程退出码 $status"
    fi
    rm -f "$PID_FILE"
    DAEMON_PID=""
}

kill_daemon_hard() {
    # set -euo pipefail 下，wait 一个被 SIGKILL 的作业返回 137 会终止整个脚本，
    # 因此这里所有 kill/wait 都必须吞掉非零退出码。
    kill -KILL "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
    if [[ -f "$PID_FILE" ]]; then
        local inner
        inner="$(cat "$PID_FILE")"
        kill -KILL "$inner" 2>/dev/null || true
        # nsenter 退出不等于内层守护进程已消失；等它真正死亡再重启，
        # 避免两个守护进程并存。已退出的进程可能是僵尸，用 ps -o stat 判定。
        local tries=0
        while kill -0 "$inner" 2>/dev/null; do
            local stat
            stat="$(ps -o stat= -p "$inner" 2>/dev/null || true)"
            [[ "$stat" == Z* ]] && break
            tries=$((tries + 1))
            if ((tries > 100)); then
                fail "崩溃模拟后守护进程 $inner 未能退出"
            fi
            sleep 0.05
        done
    fi
    rm -f "$PID_FILE"
    DAEMON_PID=""
}

[[ $(id -u) -eq 0 ]] || fail "集成测试需要以 root 运行"
[[ -x "$IPLIMIT_BIN" ]] || fail "找不到可执行文件：$IPLIMIT_BIN"
command -v iptables >/dev/null 2>&1 || fail "测试需要 iptables 来激活 conntrack"
command -v ip6tables >/dev/null 2>&1 || fail "测试需要 ip6tables 来激活 IPv6 conntrack"
command -v nsenter >/dev/null 2>&1 || fail "测试需要 nsenter（util-linux）"

# ip netns exec 会给子进程设置 PR_SET_PDEATHSIG（父进程退出即 SIGKILL），
# 因此用 nsenter + 独立包装脚本把 TERM/INT 转发给守护进程。
cat >"$RUNNER_SCRIPT" <<EOF
#!/bin/sh
trap 'kill "\$INNER" 2>/dev/null' TERM INT
env IPLIMIT_CONFIG_FILE='$CONFIG_FILE' IPLIMIT_STATE_DIR='$STATE_DIR' \\
    '$IPLIMIT_BIN' daemon-internal &
INNER=\$!
printf '%s' "\$INNER" >'$PID_FILE'
wait \$INNER
status=\$?
wait \$INNER 2>/dev/null
status=\$?
exit \$status
EOF
chmod +x "$RUNNER_SCRIPT"

log "创建独立的服务端和客户端网络命名空间"
ip netns add "$SERVER"
ip netns add "$CLIENT"
ip link add "$WAN" type veth peer name "$PEER"
ip link set "$WAN" netns "$SERVER"
ip link set "$PEER" netns "$CLIENT"
srv ip addr add 203.0.113.10/24 dev "$WAN"
srv ip -6 addr add 2001:db8:1::10/64 nodad dev "$WAN"
srv ip link set lo up
srv ip link set "$WAN" up
cli ip addr add 203.0.113.20/24 dev "$PEER"
cli ip addr add 203.0.113.30/24 dev "$PEER"
cli ip addr add 203.0.113.99/24 dev "$PEER"
cli ip -6 addr add 2001:db8:1::20/64 nodad dev "$PEER"
cli ip link set lo up
cli ip link set "$PEER" up

log "激活 conntrack 并缩短 UDP 超时"
if ! srv iptables -t nat -C POSTROUTING -o "$WAN" -j MASQUERADE >/dev/null 2>&1; then
    srv iptables -t nat -A POSTROUTING -o "$WAN" -j MASQUERADE
    ADDED_IPV4_NAT=1
fi
if ! srv ip6tables -C INPUT -m conntrack --ctstate NEW,ESTABLISHED -j ACCEPT \
    >/dev/null 2>&1; then
    srv ip6tables -A INPUT -m conntrack --ctstate NEW,ESTABLISHED -j ACCEPT
    ADDED_IPV6_CT=1
fi
lower_udp_timeout() {
    local name=$1
    local path=/proc/sys/net/netfilter/$name
    srv test -r "$path" || fail "无法读取 $path，TTL 清理测试不可靠"
    local old
    old=$(srv cat "$path")
    local var="${name^^}_OLD"
    printf -v "$var" '%s' "$old"
    srv sh -c "printf '2\n' >$path" || true
    [[ $(srv cat "$path") == "2" ]] || fail "无法把 $name 降为 2s（当前 $(srv cat "$path")s）"
}
lower_udp_timeout nf_conntrack_udp_timeout
lower_udp_timeout nf_conntrack_udp_timeout_stream

log "启动 UDP 测试端点"
srv nc -4 -u -l -s 203.0.113.10 -p 9999 >/dev/null 2>&1 &
LISTENER4=$!
srv nc -6 -u -l -s 2001:db8:1::10 -p 9999 >/dev/null 2>&1 &
LISTENER6=$!

write_config "$WAN"
rm -rf "$STATE_DIR"
log "启动守护进程"
start_daemon

wait_for "根 HTB 队列" root_htb_present
wait_for "IFB 设备" ifb_present
[[ -f "$STATE_DIR/owner.env" ]] || fail "owner.env 未写入"
[[ -f "$STATE_DIR/clients.tsv" ]] || fail "clients.tsv 未写入"
grep -q 'RULESET_VERSION=2' "$STATE_DIR/owner.env" || fail "RULESET_VERSION 不是 2"
pass "启动后创建根 HTB、clsact、IFB 和状态文件"

if srv tc filter show dev "$WAN" ingress | grep -q 'matchall'; then
    fail "不应安装全局 matchall ingress 重定向"
fi
pass "未安装全局 ingress 重定向"

log "通过 IPv4 UDP 发现客户端 203.0.113.20"
send_udp4 203.0.113.20
wait_for "clients.tsv 包含 203.0.113.20" clients_have 203.0.113.20
grep -Fq $'203.0.113.20\t1:a\t' "$STATE_DIR/clients.tsv" \
    || fail "客户端 203.0.113.20 的类别应为 1:a"
srv tc class show dev "$WAN" classid 1:a | grep -q 'htb' || fail "WAN 没有 1:a 类别"
srv tc filter show dev "$WAN" parent 1: | grep -q 'dst_ip 203.0.113.20' \
    || fail "缺少下载 flower 过滤器"
srv tc filter show dev "$IFB" parent 1: | grep -q 'src_ip 203.0.113.20' \
    || fail "缺少 IFB 上传 flower 过滤器"
srv tc filter show dev "$WAN" ingress | grep -q 'src_ip 203.0.113.20' \
    || fail "缺少 ingress 上传过滤器"
srv tc filter show dev "$WAN" ingress \
    | grep -q "mirred (Egress Redirect to device $IFB)" || fail "ingress 未重定向到 IFB"
pass "客户端 203.0.113.20 获得完整的上传/下载类别与过滤器"

clients_output=$(run_iplimit_srv clients)
grep -Fq '203.0.113.20' <<<"$clients_output" || fail "clients 命令未显示客户端"
pass "clients 命令显示已发现客户端"

status_output=$(run_iplimit_srv status)
grep -Fq "公网网卡：$WAN" <<<"$status_output" || fail "status 命令未显示网卡"
grep -Fq '203.0.113.20' <<<"$status_output" || fail "status 命令未显示客户端"
pass "status 命令显示配置与客户端"

before=$(class_packets "$IFB" 1:a)
cli ping -q -c 20 -i 0.01 -W 1 -I 203.0.113.20 203.0.113.10 >/dev/null
sleep 0.2
after=$(class_packets "$IFB" 1:a)
[[ -n "$before" && -n "$after" ]] || fail "无法读取 IFB 类别计数"
((after > before)) || fail "客户端上传流量未进入 IFB 类别"
pass "客户端上传流量进入其独立 IFB 类别"

log "验证第二个客户端与排除地址"
send_udp4 203.0.113.30
send_udp4 203.0.113.99
wait_for "clients.tsv 包含 203.0.113.30" clients_have 203.0.113.30
grep -Fq $'203.0.113.30\t1:b\t' "$STATE_DIR/clients.tsv" \
    || fail "第二个客户端类别应为 1:b"
if grep -Fq '203.0.113.99' "$STATE_DIR/clients.tsv"; then
    fail "排除地址被加入客户端列表"
fi
pass "第二个客户端独立分类，排除地址被忽略"

log "验证 IPv6 客户端"
send_udp6 2001:db8:1::20
wait_for "clients.tsv 包含 2001:db8:1::20" clients_have 2001:db8:1::20
srv tc filter show dev "$WAN" parent 1: | grep -q 'pref 49151' \
    || fail "缺少 IPv6 下载过滤器优先级"
srv tc filter show dev "$WAN" ingress | grep -q 'pref 49151' \
    || fail "缺少 IPv6 ingress 过滤器优先级"
pass "IPv6 客户端使用 49151 优先级"

log "等待空闲客户端清理"
ON_TIMEOUT='
    log "失败现场 conntrack -L：$(srv conntrack -L 2>/dev/null | tr "\n" ";")"
    log "失败现场 UDP 超时：$(srv cat /proc/sys/net/netfilter/nf_conntrack_udp_timeout) / $(srv cat /proc/sys/net/netfilter/nf_conntrack_udp_timeout_stream)"
    log "失败现场监听套接字：$(srv ss -ua 2>/dev/null | tr "\n" ";")"
    log "失败现场客户端表：$(cat "$STATE_DIR/clients.tsv" | tr "\n" ";")"
'
# 过期条目由内核 gc worker 异步删除，可能明显滞后于 nf_conntrack_udp_timeout。
# 守护进程以内核真实表为准（表中仍列出的条目视为仍活跃），因此先等表自然清空，
# 再验证 TTL 回收。不能用 conntrack -D 清表：显式删除会补发 DESTROY 事件，
# 掩盖本用例要覆盖的“事件丢失、靠 dump 核实”路径。
table_empty() {
    [[ -z "$(srv conntrack -L 2>/dev/null | grep '^udp')" ]]
}
log "当前 conntrack 表：$(srv conntrack -L -p udp 2>/dev/null | head -n 20 | tr '\n' ';')"
wait_for "内核 conntrack 表清空" --attempts 800 --on-timeout "$ON_TIMEOUT" table_empty
wait_for "clients.tsv 移除 203.0.113.20" --attempts 400 --on-timeout "$ON_TIMEOUT" clients_missing 203.0.113.20
wait_for "clients.tsv 移除 203.0.113.30" --attempts 400 --on-timeout "$ON_TIMEOUT" clients_missing 203.0.113.30
wait_for "clients.tsv 移除 2001:db8:1::20" --attempts 400 --on-timeout "$ON_TIMEOUT" clients_missing 2001:db8:1::20
if srv tc class show dev "$WAN" classid 1:a 2>/dev/null | grep -q 'htb'; then
    fail "空闲客户端的 WAN 类别未删除"
fi
pass "空闲客户端按 TTL 清理"

log "验证 SIGTERM 优雅停止与完整清理"
stop_daemon
if srv tc qdisc show dev "$WAN" | grep -q 'qdisc htb 1: root'; then
    fail "停止后根 HTB 队列未清理"
fi
if srv tc qdisc show dev "$WAN" | grep -q 'qdisc clsact'; then
    fail "停止后本工具创建的 clsact 未清理"
fi
if ifb_present; then
    fail "停止后 IFB 未删除"
fi
[[ ! -e "$STATE_DIR/owner.env" ]] || fail "停止后 owner.env 未删除"
pass "收到 SIGTERM 后清理本工具创建的规则、IFB 和状态文件"

log "模拟守护进程崩溃后重启并接管已有 tc 配置"
start_daemon
wait_for "重启后的根 HTB 队列" root_htb_present
send_udp4 203.0.113.20
wait_for "重启后重新发现客户端" clients_have 203.0.113.20
kill_daemon_hard
grep -q 'RULESET_VERSION=2' "$STATE_DIR/owner.env" \
    || fail "崩溃现场缺少 owner.env"
root_htb_present || fail "崩溃现场缺少根 HTB 队列"
start_daemon
wait_for "接管后的根 HTB 队列" root_htb_present
send_udp4 203.0.113.20
wait_for "接管后重新发现客户端" clients_have 203.0.113.20
pass "守护进程可接管崩溃残留的过滤器并重新发现客户端"

stop_daemon

log "验证拒绝覆盖外部根队列"
srv ip link add foreign0 type dummy
srv ip link set foreign0 up
srv ip addr add 192.0.2.10/24 dev foreign0
srv tc qdisc add dev foreign0 root handle 1: htb default 1
write_config foreign0
if run_iplimit_srv daemon-internal >/tmp/iplimit-foreign.log 2>&1; then
    fail "守护进程接受了外部 HTB 根队列"
fi
grep -q '拒绝替换' /tmp/iplimit-foreign.log || fail "错误信息未说明拒绝替换"
srv tc qdisc show dev foreign0 | grep -q 'qdisc htb 1: root' || fail "外部根队列被删除"
pass "拒绝覆盖外部根队列并保持原配置"

trap - EXIT
cleanup
printf 'tc/ctnetlink 集成测试全部通过\n'
