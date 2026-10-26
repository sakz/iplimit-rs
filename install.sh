#!/usr/bin/env bash
# iplimit-rs 一键安装：
#   curl -fsSL https://raw.githubusercontent.com/sakz/iplimit-rs/main/install.sh | sudo bash
# 默认下载 GitHub Release 中对应架构的预编译二进制；
# IPLIMIT_FROM_SOURCE=1 或找不到预编译产物时回退到 cargo 源码构建。
set -Eeuo pipefail

readonly REPO_SLUG="${IPLIMIT_REPO:-sakz/iplimit-rs}"
readonly REPO_REF="${IPLIMIT_REF:-main}"
readonly RELEASE_TAG="${IPLIMIT_RELEASE_TAG:-latest}"
readonly RAW_BASE="https://raw.githubusercontent.com/${REPO_SLUG}/${REPO_REF}"
readonly GITHUB_API="https://api.github.com/repos/${REPO_SLUG}"
readonly DESTDIR="${DESTDIR:-}"
SCRIPT_PATH=$0
SCRIPT_DIR=$(cd "$(dirname "$SCRIPT_PATH")" && pwd)
readonly SCRIPT_DIR
INSTALL_WORK_DIR=""

log() {
    printf '[iplimit 安装器] %s\n' "$*"
}

die() {
    printf '[iplimit 安装器] 错误：%s\n' "$*" >&2
    exit 1
}

fetch_url() {
    local url=$1
    local destination=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$url" -o "$destination"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$destination" "$url"
    else
        die "下载 $url 需要 curl 或 wget"
    fi
}

fetch_file() {
    local name=$1
    local destination=$2
    if [[ -f "$SCRIPT_DIR/$name" && "$SCRIPT_DIR/$name" != "$destination" ]]; then
        cp "$SCRIPT_DIR/$name" "$destination"
    else
        fetch_url "$RAW_BASE/$name" "$destination"
    fi
}

detect_arch() {
    local machine
    machine=$(uname -m)
    case "$machine" in
        x86_64 | amd64) printf 'amd64' ;;
        aarch64 | arm64) printf 'arm64' ;;
        *) die "不支持的 CPU 架构：$machine（可尝试 IPLIMIT_FROM_SOURCE=1 源码安装）" ;;
    esac
}

install_dependencies() {
    [[ ${SKIP_DEPENDENCIES:-0} == "1" ]] && return
    command -v apt-get >/dev/null 2>&1 || die "自动安装依赖仅支持 Debian/Ubuntu；其他系统请手动准备 iproute2、conntrack 用户态工具、kmod、systemd 后使用 SKIP_DEPENDENCIES=1"
    log "正在安装依赖（iproute2、conntrack、kmod、systemd）"
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends iproute2 conntrack kmod systemd ca-certificates curl tar
}

# 解析要下载的 release 资源名；RELEASE_TAG=latest 时查询 GitHub API。
# 失败返回非零（不能在命令替换里 die，否则会绕过调用方的 if !）。
release_binary_url() {
    local arch=$1
    local asset="iplimit-linux-${arch}"
    local tag=$RELEASE_TAG
    if [[ "$RELEASE_TAG" == "latest" ]]; then
        command -v curl >/dev/null 2>&1 || return 1
        tag=$(curl -fsSL "$GITHUB_API/releases/latest" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
        [[ -n "$tag" ]] || return 1
    fi
    printf 'https://github.com/%s/releases/download/%s/%s' "$REPO_SLUG" "$tag" "$asset"
}

build_from_source() {
    command -v cargo >/dev/null 2>&1 || die "源码安装需要 cargo（先安装 rustup 再重试），或改用默认的二进制安装"
    log "正在从源码构建（cargo build --release）"
    local archive="$INSTALL_WORK_DIR/repo.tar.gz"
    fetch_url "https://github.com/${REPO_SLUG}/archive/refs/heads/${REPO_REF}.tar.gz" "$archive"
    tar -xzf "$archive" -C "$INSTALL_WORK_DIR"
    local slug_dir=${REPO_SLUG##*-}
    slug_dir=${slug_dir//_/-}
    local tree
    tree=$(find "$INSTALL_WORK_DIR" -maxdepth 1 -type d -name "${slug_dir}-*")
    (cd "$tree" && cargo build --release)
    cp "$tree/target/release/iplimit" "$INSTALL_WORK_DIR/iplimit"
}

atomic_install() {
    local source_file=$1
    local destination=$2
    local mode=$3
    local tmp="${destination}.tmp.$$"
    install -m "$mode" "$source_file" "$tmp"
    mv -f "$tmp" "$destination"
}

main() {
    if [[ -z "$DESTDIR" && $(id -u) -ne 0 ]]; then
        die "请使用 root 权限运行，例如：curl ... | sudo bash"
    fi

    install_dependencies

    local bin_dir="$DESTDIR/usr/local/bin"
    local unit_dir="$DESTDIR/etc/systemd/system"
    local config_dir="$DESTDIR/etc"
    INSTALL_WORK_DIR=$(mktemp -d)
    trap 'rm -rf "$INSTALL_WORK_DIR"' EXIT

    install -d -m 755 "$bin_dir" "$unit_dir" "$config_dir"

    local arch
    arch=$(detect_arch)
    log "检测到架构：linux-$arch"
    if [[ -f "$SCRIPT_DIR/iplimit" && ${IPLIMIT_FROM_SOURCE:-0} != "1" ]]; then
        log "使用与安装脚本同目录的 iplimit 二进制（克隆仓库安装）"
        cp "$SCRIPT_DIR/iplimit" "$INSTALL_WORK_DIR/iplimit"
    elif [[ ${IPLIMIT_FROM_SOURCE:-0} == "1" ]]; then
        build_from_source
    else
        log "正在下载预编译二进制"
        local url
        if url=$(release_binary_url "$arch") \
            && fetch_url "$url" "$INSTALL_WORK_DIR/iplimit" \
            && chmod +x "$INSTALL_WORK_DIR/iplimit" \
            && "$INSTALL_WORK_DIR/iplimit" version >/dev/null; then
            :
        else
            log "预编译二进制不可用或无法在本机运行，回退到源码构建"
            rm -f "$INSTALL_WORK_DIR/iplimit"
            build_from_source
        fi
    fi
    chmod +x "$INSTALL_WORK_DIR/iplimit"
    "$INSTALL_WORK_DIR/iplimit" version

    fetch_file iplimit.service "$INSTALL_WORK_DIR/iplimit.service"

    atomic_install "$INSTALL_WORK_DIR/iplimit" "$bin_dir/iplimit" 755
    atomic_install "$INSTALL_WORK_DIR/iplimit.service" "$unit_dir/iplimit.service" 644

    if [[ ! -e "$config_dir/iplimit.conf" ]]; then
        cat >"$config_dir/iplimit.conf" <<'EOF'
# 由 iplimit 管理。修改后请运行：sudo iplimit start
RATE_MBIT="20"
INTERFACE="auto"
IFB_NAME="ifb-iplimit"
POLL_INTERVAL="2"
CLIENT_TTL="300"
UNSHAPED_RATE_MBIT="100000"
EXCLUDE_IPS=""
EOF
        chmod 600 "$config_dir/iplimit.conf"
    fi

    # 从旧版 bash 实现升级时接管其运行状态。
    if [[ -z "$DESTDIR" ]]; then
        systemctl daemon-reload
        if systemctl is-active --quiet iplimit.service; then
            systemctl restart iplimit.service
            log "已升级并重启正在运行的服务"
        fi
    fi

    log "已安装到 /usr/local/bin/iplimit"
    log "运行交互式菜单：sudo iplimit"
    log "或直接启动：sudo iplimit start 20"
}

main "$@"
