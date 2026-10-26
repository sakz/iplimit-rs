use std::collections::{HashMap, HashSet};
use std::io;
use std::mem;
use std::net::IpAddr;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::bail;
use crate::config::Config;
use crate::conntrack::{parse_datagram, ConntrackSocket, CtEvent, CtMessage, CtTuple, DumpResult};
use crate::error::Result;
use crate::log::{log, warn};
use crate::network::{
    add_client_class, cleanup_network, detect_interface, local_addresses, remove_client_class,
    require_commands, require_root, setup_network,
};
use crate::notify;
use crate::state::write_clients;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);
const RESYNC_RETRY: Duration = Duration::from_secs(30);
const MAX_DATAGRAMS_PER_WAKE: usize = 512;
const FIRST_CLASS_ID: u16 = 10;
const LAST_CLASS_ID: u16 = 65000;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_UDP: u8 = 17;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let fd = SIGNAL_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = 1u8;
        unsafe {
            libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

struct SignalPipe {
    read_fd: RawFd,
    write_fd: RawFd,
}

impl SignalPipe {
    fn install() -> Result<Self> {
        let mut fds = [-1i32; 2];
        let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if result < 0 {
            bail!("创建信号管道失败：{}", io::Error::last_os_error());
        }
        let pipe = Self {
            read_fd: fds[0],
            write_fd: fds[1],
        };
        SIGNAL_WRITE_FD.store(pipe.write_fd, Ordering::SeqCst);

        let mut action: libc::sigaction = unsafe { mem::zeroed() };
        action.sa_sigaction = handle_signal as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        for signal in [libc::SIGTERM, libc::SIGINT] {
            let result = unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
            if result < 0 {
                bail!("安装信号处理器失败：{}", io::Error::last_os_error());
            }
        }
        Ok(pipe)
    }

    fn fd(&self) -> RawFd {
        self.read_fd
    }

    fn should_stop(&self) -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }

    fn drain(&self) {
        let mut buffer = [0u8; 64];
        loop {
            let received = unsafe {
                libc::read(
                    self.read_fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if received <= 0 {
                break;
            }
        }
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

struct NetworkGuard {
    config: Config,
    device: String,
    armed: bool,
}

impl NetworkGuard {
    fn new(config: &Config, device: &str) -> Self {
        Self {
            config: config.clone(),
            device: device.to_string(),
            armed: true,
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        cleanup_network(&self.config, Some(&self.device))
    }
}

impl Drop for NetworkGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = cleanup_network(&self.config, Some(&self.device));
        }
    }
}

struct ClientState {
    class_id: u16,
    last_seen: u64,
}

struct Daemon {
    config: Config,
    device: String,
    local: HashSet<IpAddr>,
    excluded: HashSet<IpAddr>,
    ip_class: HashMap<IpAddr, ClientState>,
    class_ip: HashMap<u16, IpAddr>,
    connections: HashMap<IpAddr, u32>,
    pending: HashMap<IpAddr, u64>,
    dirty: bool,
    next_reconcile: Instant,
}

impl Daemon {
    fn new(
        config: Config,
        device: String,
        local: HashSet<IpAddr>,
        excluded: HashSet<IpAddr>,
        next_reconcile: Instant,
    ) -> Self {
        Self {
            config,
            device,
            local,
            excluded,
            ip_class: HashMap::new(),
            class_ip: HashMap::new(),
            connections: HashMap::new(),
            pending: HashMap::new(),
            dirty: false,
            next_reconcile,
        }
    }

    fn apply_dump(&mut self, dump: DumpResult, now: u64) {
        let counts = dump.counts;
        self.connections
            .retain(|address, _| counts.contains_key(address));
        let mut addresses: Vec<(IpAddr, u32)> = counts.into_iter().collect();
        addresses.sort_by_key(|(address, _)| *address);
        for (address, count) in addresses {
            self.connections.insert(address, count);
            self.touch(address, now);
        }
        for message in dump.events {
            self.handle_message(message, now);
        }
        self.dirty = true;
    }

    fn handle_message(&mut self, message: CtMessage, now: u64) -> bool {
        match message {
            CtMessage::Entry(tuple) => {
                self.ensure_active(&tuple, now);
                false
            }
            CtMessage::Event {
                kind: CtEvent::New,
                tuple,
            } => {
                if self.is_client(&tuple) {
                    let counter = self.connections.entry(tuple.src).or_insert(0);
                    *counter = counter.saturating_add(1);
                    self.touch(tuple.src, now);
                }
                false
            }
            CtMessage::Event {
                kind: CtEvent::Update,
                tuple,
            } => {
                self.ensure_active(&tuple, now);
                false
            }
            CtMessage::Event {
                kind: CtEvent::Destroy,
                tuple,
            } => {
                if self.is_client(&tuple) {
                    let exhausted = if let Some(counter) = self.connections.get_mut(&tuple.src) {
                        *counter = counter.saturating_sub(1);
                        *counter == 0
                    } else {
                        false
                    };
                    if exhausted {
                        self.connections.remove(&tuple.src);
                    }
                }
                false
            }
            CtMessage::Error(code) => {
                if code == -libc::ENOBUFS {
                    warn("netlink 事件缓冲区溢出，正在重新同步 conntrack");
                    true
                } else {
                    let error = io::Error::from_raw_os_error(-code);
                    warn(format!("ctnetlink 返回错误：{error}"));
                    false
                }
            }
            CtMessage::Overrun => {
                warn("netlink 事件缓冲区溢出，正在重新同步 conntrack");
                true
            }
            CtMessage::Done { .. } => false,
        }
    }

    fn is_client(&self, tuple: &CtTuple) -> bool {
        is_client_tuple(tuple, &self.local, &self.excluded)
    }

    fn ensure_active(&mut self, tuple: &CtTuple, now: u64) {
        if self.is_client(tuple) {
            self.connections.entry(tuple.src).or_insert(1);
            self.touch(tuple.src, now);
        }
    }

    fn touch(&mut self, address: IpAddr, now: u64) {
        if let Some(state) = self.ip_class.get_mut(&address) {
            state.last_seen = now;
            self.dirty = true;
            return;
        }
        if let Some(last_seen) = self.pending.get_mut(&address) {
            *last_seen = now;
            return;
        }
        self.try_add(address, now);
    }

    fn try_add(&mut self, address: IpAddr, now: u64) {
        if self.ip_class.contains_key(&address) {
            return;
        }
        let Some(class_id) = self.allocate_class() else {
            warn(format!("没有可供客户端 {address} 使用的 tc 类别 ID"));
            self.pending.entry(address).or_insert(now);
            return;
        };
        match add_client_class(&self.config, &self.device, address, class_id) {
            Ok(()) => {
                self.ip_class.insert(
                    address,
                    ClientState {
                        class_id,
                        last_seen: now,
                    },
                );
                self.class_ip.insert(class_id, address);
                self.pending.remove(&address);
                self.dirty = true;
                log(format!("已添加客户端 {address}，类别 1:{class_id:x}"));
            }
            Err(error) => {
                warn(format!("无法为客户端 {address} 添加 tc 类别：{error}"));
                self.pending.entry(address).or_insert(now);
            }
        }
    }

    fn allocate_class(&self) -> Option<u16> {
        (FIRST_CLASS_ID..=LAST_CLASS_ID).find(|candidate| !self.class_ip.contains_key(candidate))
    }

    fn tick(&mut self, socket: &mut ConntrackSocket) {
        let now = unix_now();
        let ttl = self.config.client_ttl_secs();

        self.verify_idle(socket, now, ttl);

        let active: Vec<IpAddr> = self
            .ip_class
            .keys()
            .filter(|address| self.connections.contains_key(*address))
            .copied()
            .collect();
        if !active.is_empty() {
            self.dirty = true;
        }
        for address in active {
            if let Some(state) = self.ip_class.get_mut(&address) {
                state.last_seen = now;
            }
        }

        let pending: Vec<(IpAddr, u64)> = self
            .pending
            .iter()
            .map(|(address, last_seen)| (*address, *last_seen))
            .collect();
        for (address, last_seen) in pending {
            if !self.connections.contains_key(&address) && now.saturating_sub(last_seen) > ttl {
                self.pending.remove(&address);
                continue;
            }
            self.try_add(address, now);
        }

        let mut idle: Vec<(IpAddr, u16)> = self
            .ip_class
            .iter()
            .filter(|(address, state)| {
                !self.connections.contains_key(*address)
                    && now.saturating_sub(state.last_seen) > ttl
            })
            .map(|(address, state)| (*address, state.class_id))
            .collect();
        idle.sort();
        for (address, class_id) in idle {
            self.remove(address, class_id);
        }

        self.flush_clients();
    }

    /// DESTROY 事件可能因内核事件扩展的惰性分配而丢失（条目建档时没有监听器
    /// 就不会补发），此时连接计数会虚高。对「TTL 已超时但计数仍 >0」的客户端
    /// 用一次定向 dump 以内核真实表核实，避免永久误判为活跃。
    fn verify_idle(&mut self, socket: &mut ConntrackSocket, now: u64, ttl: u64) {
        let suspects: Vec<IpAddr> = self
            .connections
            .iter()
            .filter(|(address, _)| match self.ip_class.get(*address) {
                Some(state) => now.saturating_sub(state.last_seen) > ttl,
                None => false,
            })
            .map(|(address, _)| *address)
            .collect();
        if suspects.is_empty() {
            return;
        }
        match socket.blocking_dump(|tuple| is_client_tuple(tuple, &self.local, &self.excluded)) {
            Ok(dump) => self.apply_dump(dump, now),
            Err(error) => warn(format!("空闲核实 dump 失败，本轮跳过：{error}")),
        }
    }

    fn remove(&mut self, address: IpAddr, class_id: u16) {
        remove_client_class(&self.config, &self.device, address, class_id);
        self.ip_class.remove(&address);
        self.class_ip.remove(&class_id);
        self.dirty = true;
        log(format!("已移除空闲客户端 {address}"));
    }

    fn drain_events(&mut self, socket: &mut ConntrackSocket) -> Result<()> {
        let mut processed = 0usize;
        while processed < MAX_DATAGRAMS_PER_WAKE {
            match socket.try_receive() {
                Ok(Some(buffer)) => {
                    let now = unix_now();
                    let mut resync = false;
                    for message in parse_datagram(buffer) {
                        if self.handle_message(message, now) {
                            resync = true;
                        }
                    }
                    processed += 1;
                    if resync {
                        self.resync(socket);
                    }
                }
                Ok(None) => break,
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                    warn("netlink 事件缓冲区溢出，正在重新同步 conntrack");
                    self.resync(socket);
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.flush_clients();
        Ok(())
    }

    fn reconcile(&mut self, socket: &mut ConntrackSocket) {
        match local_addresses(&self.device) {
            Ok(addresses) if !addresses.is_empty() => self.local = addresses,
            Ok(_) => warn(format!(
                "网卡 {} 上没有找到全局 IP 地址，继续使用上次的地址",
                self.device
            )),
            Err(error) => warn(format!("刷新网卡地址失败：{error}")),
        }

        let now = unix_now();
        match socket.blocking_dump(|tuple| is_client_tuple(tuple, &self.local, &self.excluded)) {
            Ok(dump) => {
                self.apply_dump(dump, now);
                self.next_reconcile = Instant::now() + RECONCILE_INTERVAL;
            }
            Err(error) => {
                warn(format!("重新同步 conntrack 失败：{error}"));
                self.next_reconcile = Instant::now() + RESYNC_RETRY;
            }
        }
        self.flush_clients();
    }

    fn resync(&mut self, socket: &mut ConntrackSocket) {
        let now = unix_now();
        match socket.blocking_dump(|tuple| is_client_tuple(tuple, &self.local, &self.excluded)) {
            Ok(dump) => {
                self.apply_dump(dump, now);
                self.next_reconcile = Instant::now() + RECONCILE_INTERVAL;
            }
            Err(error) => {
                warn(format!("重新同步 conntrack 失败：{error}"));
                self.next_reconcile = Instant::now() + RESYNC_RETRY;
            }
        }
        self.flush_clients();
    }

    fn flush_clients(&mut self) {
        if !self.dirty {
            return;
        }
        let mut rows: Vec<(String, u16, u64)> = self
            .ip_class
            .iter()
            .map(|(address, state)| (address.to_string(), state.class_id, state.last_seen))
            .collect();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        match write_clients(&self.config.clients_file(), &rows) {
            Ok(()) => self.dirty = false,
            Err(error) => warn(format!("写入客户端状态失败：{error}")),
        }
    }
}

struct Ready {
    netlink: bool,
    signal: bool,
}

fn wait_for_events(netlink_fd: RawFd, signal_fd: RawFd, timeout: Duration) -> io::Result<Ready> {
    let mut fds = [
        libc::pollfd {
            fd: netlink_fd,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: signal_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
    let result = unsafe { libc::poll(fds.as_mut_ptr(), 2, milliseconds) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(Ready {
                netlink: false,
                signal: true,
            });
        }
        return Err(error);
    }
    Ok(Ready {
        netlink: fds[0].revents & libc::POLLIN != 0,
        signal: fds[1].revents & libc::POLLIN != 0,
    })
}

fn is_client_tuple(tuple: &CtTuple, local: &HashSet<IpAddr>, excluded: &HashSet<IpAddr>) -> bool {
    matches!(tuple.protocol, IPPROTO_TCP | IPPROTO_UDP)
        && local.contains(&tuple.dst)
        && !local.contains(&tuple.src)
        && !excluded.contains(&tuple.src)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn run(config: Config) -> Result<()> {
    require_root()?;
    require_commands(&["ip", "tc"])?;
    config.validate()?;

    let device = detect_interface(&config)?;
    let local = local_addresses(&device)?;
    if local.is_empty() {
        bail!("网卡 {device} 上没有找到全局 IP 地址");
    }
    let excluded = config.excluded_set();

    let signals = SignalPipe::install()?;
    let mut socket = ConntrackSocket::open()?;

    let startup = socket.blocking_dump(|tuple| is_client_tuple(tuple, &local, &excluded));
    if signals.should_stop() {
        return Ok(());
    }

    let mut daemon = Daemon::new(
        config,
        device,
        local,
        excluded,
        Instant::now() + RECONCILE_INTERVAL,
    );

    setup_network(&daemon.config, &daemon.device)?;
    let mut network = NetworkGuard::new(&daemon.config, &daemon.device);
    if signals.should_stop() {
        let _ = notify::stopping("正在停止并清理 tc 配置");
        network.cleanup()?;
        return Ok(());
    }

    let status = format!(
        "正在 {} 上按客户端限制为 {} Mbps",
        daemon.device, daemon.config.rate_mbit
    );
    if let Err(error) = notify::ready(&status) {
        warn(format!("无法通知 systemd 服务已就绪：{error}"));
    }

    match startup {
        Ok(dump) => daemon.apply_dump(dump, unix_now()),
        Err(error) => {
            warn(format!(
                "首次读取 conntrack 失败，将依赖事件继续运行：{error}"
            ));
            daemon.next_reconcile = Instant::now() + RESYNC_RETRY;
        }
    }
    daemon.flush_clients();
    log(format!(
        "已在 {} 启用：每个客户端每个方向 {} Mbps",
        daemon.device, daemon.config.rate_mbit
    ));

    let tick_duration = Duration::from_secs(daemon.config.poll_interval_secs());
    let mut next_tick = Instant::now() + tick_duration;

    while !signals.should_stop() {
        if Instant::now() >= daemon.next_reconcile {
            daemon.reconcile(&mut socket);
        }
        if Instant::now() >= next_tick {
            daemon.tick(&mut socket);
            next_tick = Instant::now() + tick_duration;
        }
        if signals.should_stop() {
            break;
        }

        let deadline = next_tick.min(daemon.next_reconcile);
        let timeout = deadline.saturating_duration_since(Instant::now());
        let ready = wait_for_events(socket.fd(), signals.fd(), timeout)?;
        if ready.signal {
            signals.drain();
        }
        if signals.should_stop() {
            break;
        }
        if ready.netlink {
            daemon.drain_events(&mut socket)?;
        }
    }

    let _ = notify::stopping("正在停止并清理 tc 配置");
    network.cleanup()?;
    log("已停止，并清理 IPLimit 创建的 tc 规则和 IFB 设备");
    Ok(())
}
