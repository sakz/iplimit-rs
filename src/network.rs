use std::collections::HashSet;
use std::env;
use std::fs;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use crate::bail;
use crate::config::Config;
use crate::error::Result;
use crate::state::{read_owner, remove_state_files, write_owner, Owner};

pub const IPV4_FILTER_PREF: u16 = 49150;
pub const IPV6_FILTER_PREF: u16 = 49151;
pub const LEGACY_REDIRECT_PREF: u16 = 49152;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpec {
    pub protocol: &'static str,
    pub filter_priority: u16,
    pub handle: String,
    pub class_hex: String,
    pub download_cidr: String,
    pub upload_cidr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: &'static str,
    pub args: Vec<String>,
}

impl CommandSpec {
    fn tc(args: &[String]) -> Self {
        Self {
            program: "tc",
            args: args.to_vec(),
        }
    }
}

pub fn client_spec(ip: IpAddr, class_id: u16) -> ClientSpec {
    let (protocol, filter_priority, suffix) = match ip {
        IpAddr::V4(_) => ("ip", IPV4_FILTER_PREF, 32),
        IpAddr::V6(_) => ("ipv6", IPV6_FILTER_PREF, 128),
    };
    ClientSpec {
        protocol,
        filter_priority,
        handle: format!("0x{class_id:x}"),
        class_hex: format!("{class_id:x}"),
        download_cidr: format!("{ip}/{suffix}"),
        upload_cidr: format!("{ip}/{suffix}"),
    }
}

pub fn add_client_commands(config: &Config, device: &str, spec: &ClientSpec) -> Vec<CommandSpec> {
    let classid = format!("1:{}", spec.class_hex);
    let parent = format!("1:{}", spec.class_hex);
    let handle = format!("{}:", spec.class_hex);
    let rate = format!("{}mbit", config.rate_mbit);
    let pref = spec.filter_priority.to_string();
    let class_part = vec![
        "class".to_string(),
        "add".to_string(),
        "dev".to_string(),
        device.to_string(),
        "parent".to_string(),
        "1:".to_string(),
        "classid".to_string(),
        classid.clone(),
        "htb".to_string(),
        "rate".to_string(),
        rate.clone(),
        "ceil".to_string(),
        rate.clone(),
        "quantum".to_string(),
        "1514".to_string(),
    ];
    let qdisc_part = vec![
        "qdisc".to_string(),
        "add".to_string(),
        "dev".to_string(),
        device.to_string(),
        "parent".to_string(),
        parent.clone(),
        "handle".to_string(),
        handle.clone(),
        "fq_codel".to_string(),
    ];
    let wan_filter = vec![
        "filter".to_string(),
        "add".to_string(),
        "dev".to_string(),
        device.to_string(),
        "parent".to_string(),
        "1:".to_string(),
        "pref".to_string(),
        pref.clone(),
        "protocol".to_string(),
        spec.protocol.to_string(),
        "handle".to_string(),
        spec.handle.clone(),
        "flower".to_string(),
        "dst_ip".to_string(),
        spec.download_cidr.clone(),
        "classid".to_string(),
        classid.clone(),
    ];
    let ifb_class = vec![
        "class".to_string(),
        "add".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
        "parent".to_string(),
        "1:".to_string(),
        "classid".to_string(),
        classid.clone(),
        "htb".to_string(),
        "rate".to_string(),
        rate.clone(),
        "ceil".to_string(),
        rate.clone(),
        "quantum".to_string(),
        "1514".to_string(),
    ];
    let ifb_qdisc = vec![
        "qdisc".to_string(),
        "add".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
        "parent".to_string(),
        parent,
        "handle".to_string(),
        handle,
        "fq_codel".to_string(),
    ];
    let ifb_filter = vec![
        "filter".to_string(),
        "add".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
        "parent".to_string(),
        "1:".to_string(),
        "pref".to_string(),
        pref.clone(),
        "protocol".to_string(),
        spec.protocol.to_string(),
        "handle".to_string(),
        spec.handle.clone(),
        "flower".to_string(),
        "src_ip".to_string(),
        spec.upload_cidr.clone(),
        "classid".to_string(),
        classid,
    ];
    let ingress_filter = vec![
        "filter".to_string(),
        "add".to_string(),
        "dev".to_string(),
        device.to_string(),
        "ingress".to_string(),
        "pref".to_string(),
        pref,
        "protocol".to_string(),
        spec.protocol.to_string(),
        "handle".to_string(),
        spec.handle.clone(),
        "flower".to_string(),
        "src_ip".to_string(),
        spec.upload_cidr.clone(),
        "action".to_string(),
        "mirred".to_string(),
        "egress".to_string(),
        "redirect".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
    ];
    vec![
        CommandSpec::tc(&class_part),
        CommandSpec::tc(&qdisc_part),
        CommandSpec::tc(&wan_filter),
        CommandSpec::tc(&ifb_class),
        CommandSpec::tc(&ifb_qdisc),
        CommandSpec::tc(&ifb_filter),
        CommandSpec::tc(&ingress_filter),
    ]
}

pub fn remove_client_commands(
    config: &Config,
    device: &str,
    ip: IpAddr,
    class_id: u16,
) -> Vec<CommandSpec> {
    let spec = client_spec(ip, class_id);
    let classid = format!("1:{}", spec.class_hex);
    let parent = format!("1:{}", spec.class_hex);
    let pref = spec.filter_priority.to_string();
    let ingress_del = vec![
        "filter".to_string(),
        "del".to_string(),
        "dev".to_string(),
        device.to_string(),
        "ingress".to_string(),
        "pref".to_string(),
        pref.clone(),
        "protocol".to_string(),
        spec.protocol.to_string(),
        "handle".to_string(),
        spec.handle.clone(),
        "flower".to_string(),
    ];
    let wan_filter_del = vec![
        "filter".to_string(),
        "del".to_string(),
        "dev".to_string(),
        device.to_string(),
        "parent".to_string(),
        "1:".to_string(),
        "pref".to_string(),
        pref.clone(),
        "protocol".to_string(),
        spec.protocol.to_string(),
        "handle".to_string(),
        spec.handle.clone(),
        "flower".to_string(),
    ];
    let wan_qdisc_del = vec![
        "qdisc".to_string(),
        "del".to_string(),
        "dev".to_string(),
        device.to_string(),
        "parent".to_string(),
        parent.clone(),
    ];
    let wan_class_del = vec![
        "class".to_string(),
        "del".to_string(),
        "dev".to_string(),
        device.to_string(),
        "classid".to_string(),
        classid.clone(),
    ];
    let ifb_filter_del = vec![
        "filter".to_string(),
        "del".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
        "parent".to_string(),
        "1:".to_string(),
        "pref".to_string(),
        pref.clone(),
        "protocol".to_string(),
        spec.protocol.to_string(),
        "handle".to_string(),
        spec.handle.clone(),
        "flower".to_string(),
    ];
    let ifb_qdisc_del = vec![
        "qdisc".to_string(),
        "del".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
        "parent".to_string(),
        parent,
    ];
    let ifb_class_del = vec![
        "class".to_string(),
        "del".to_string(),
        "dev".to_string(),
        config.ifb_name.clone(),
        "classid".to_string(),
        classid,
    ];
    vec![
        CommandSpec::tc(&ingress_del),
        CommandSpec::tc(&wan_filter_del),
        CommandSpec::tc(&wan_qdisc_del),
        CommandSpec::tc(&wan_class_del),
        CommandSpec::tc(&ifb_filter_del),
        CommandSpec::tc(&ifb_qdisc_del),
        CommandSpec::tc(&ifb_class_del),
    ]
}

pub fn add_client_class(config: &Config, device: &str, ip: IpAddr, class_id: u16) -> Result<()> {
    let spec = client_spec(ip, class_id);
    for command in add_client_commands(config, device, &spec) {
        if let Err(error) = run_command_spec(&command) {
            remove_client_class(config, device, ip, class_id);
            return Err(error);
        }
    }
    Ok(())
}

pub fn remove_client_class(config: &Config, device: &str, ip: IpAddr, class_id: u16) {
    for command in remove_client_commands(config, device, ip, class_id) {
        let _ = run_command_spec_ok(&command);
    }
}

pub fn setup_network(config: &Config, device: &str) -> Result<()> {
    fs::create_dir_all(&config.state_dir).map_err(|error| {
        format!(
            "创建状态目录 {} 失败：{}",
            config.state_dir.display(),
            error
        )
    })?;

    let owner = read_owner(&config.owner_file());
    assert_safe_root_qdisc(device, owner.as_ref())?;

    let previous_owner = owner
        .as_ref()
        .map(|value| value.interface.as_str())
        .unwrap_or("");
    let previous_ruleset = owner
        .as_ref()
        .and_then(|value| value.ruleset_version.as_deref());
    let filters_occupied = priority_has_filters(device, IPV4_FILTER_PREF)
        || priority_has_filters(device, IPV6_FILTER_PREF);
    let mut reclaim_new_filters = false;
    if filters_occupied {
        if previous_owner == device && previous_ruleset == Some("2") {
            reclaim_new_filters = true;
        } else {
            bail!("{device} 的入口过滤器优先级 {IPV4_FILTER_PREF}/{IPV6_FILTER_PREF} 已被占用");
        }
    }
    let reclaim_legacy_filter = previous_owner == device
        && previous_ruleset.is_none()
        && legacy_redirect_matches(device, &config.ifb_name);

    let previous_created_clsact = owner
        .as_ref()
        .map(|value| value.created_clsact)
        .unwrap_or(false);

    if link_exists(&config.ifb_name) {
        let owned_ifb = owner
            .as_ref()
            .map(|value| value.ifb_name.as_str())
            .unwrap_or("");
        if owned_ifb != config.ifb_name {
            bail!("IFB 设备已存在且不属于 IPLimit：{}", config.ifb_name);
        }
    }

    let mut guard = SetupGuard {
        config,
        device,
        armed: false,
    };
    guard.armed = true;

    if reclaim_new_filters {
        let pref4 = IPV4_FILTER_PREF.to_string();
        let pref6 = IPV6_FILTER_PREF.to_string();
        let _ = run_ok(
            "tc",
            &[
                "filter", "del", "dev", device, "ingress", "pref", &pref4, "protocol", "ip",
            ],
        );
        let _ = run_ok(
            "tc",
            &[
                "filter", "del", "dev", device, "ingress", "pref", &pref6, "protocol", "ipv6",
            ],
        );
    }
    if reclaim_legacy_filter {
        let pref = LEGACY_REDIRECT_PREF.to_string();
        let _ = run_ok(
            "tc",
            &[
                "filter", "del", "dev", device, "ingress", "pref", &pref, "protocol", "all",
            ],
        );
    }

    write_owner(
        &config.owner_file(),
        device,
        &config.ifb_name,
        previous_created_clsact,
    )?;

    if !link_exists(&config.ifb_name) {
        let _ = run_ok("modprobe", &["ifb"]);
        run_checked(
            "ip",
            &["link", "add", config.ifb_name.as_str(), "type", "ifb"],
        )?;
    }
    run_checked(
        "ip",
        &["link", "set", "dev", config.ifb_name.as_str(), "up"],
    )?;

    let clsact_exists = qdisc_show(device)
        .map(|text| text.lines().any(|line| line.starts_with("qdisc clsact ")))
        .unwrap_or(false);
    let created_clsact = if clsact_exists {
        previous_created_clsact
    } else {
        run_checked("tc", &["qdisc", "add", "dev", device, "clsact"])?;
        true
    };
    write_owner(
        &config.owner_file(),
        device,
        &config.ifb_name,
        created_clsact,
    )?;

    let unshaped = format!("{}mbit", config.unshaped_rate_mbit);
    // 崩溃重启接管时 htb 根队列仍然存在；部分内核（至少 6.8）对已存在的 htb
    // 执行 replace 会报 "Change operation not supported"，因此先删后建。
    ensure_shaped_root(device)?;
    run_checked(
        "tc",
        &[
            "class",
            "replace",
            "dev",
            device,
            "parent",
            "1:",
            "classid",
            "1:1",
            "htb",
            "rate",
            unshaped.as_str(),
            "ceil",
            unshaped.as_str(),
            "quantum",
            "15140",
        ],
    )?;
    run_checked(
        "tc",
        &[
            "qdisc", "replace", "dev", device, "parent", "1:1", "handle", "2:", "fq_codel",
        ],
    )?;

    ensure_shaped_root(&config.ifb_name)?;
    run_checked(
        "tc",
        &[
            "class",
            "replace",
            "dev",
            config.ifb_name.as_str(),
            "parent",
            "1:",
            "classid",
            "1:1",
            "htb",
            "rate",
            unshaped.as_str(),
            "ceil",
            unshaped.as_str(),
            "quantum",
            "15140",
        ],
    )?;
    run_checked(
        "tc",
        &[
            "qdisc",
            "replace",
            "dev",
            config.ifb_name.as_str(),
            "parent",
            "1:1",
            "handle",
            "2:",
            "fq_codel",
        ],
    )?;

    guard.armed = false;
    Ok(())
}

fn ensure_shaped_root(device: &str) -> Result<()> {
    run_ok(
        "tc",
        &["qdisc", "del", "dev", device, "root", "handle", "1:"],
    );
    run_checked(
        "tc",
        &[
            "qdisc", "add", "dev", device, "root", "handle", "1:", "htb", "default", "1",
        ],
    )
}

struct SetupGuard<'a> {
    config: &'a Config,
    device: &'a str,
    armed: bool,
}

impl Drop for SetupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = cleanup_network(self.config, Some(self.device));
        }
    }
}

pub fn cleanup_network(config: &Config, device: Option<&str>) -> Result<()> {
    let Some(owner) = read_owner(&config.owner_file()) else {
        return Ok(());
    };

    let device = match device {
        Some(value) => Some(value.to_string()),
        None if !owner.interface.is_empty() => Some(owner.interface.clone()),
        None => None,
    };

    if let Some(device) = device {
        if link_exists(&device) {
            if owner.ruleset_version.as_deref() == Some("2") {
                let pref4 = IPV4_FILTER_PREF.to_string();
                let pref6 = IPV6_FILTER_PREF.to_string();
                let _ = run_ok(
                    "tc",
                    &[
                        "filter", "del", "dev", &device, "ingress", "pref", &pref4, "protocol",
                        "ip",
                    ],
                );
                let _ = run_ok(
                    "tc",
                    &[
                        "filter", "del", "dev", &device, "ingress", "pref", &pref6, "protocol",
                        "ipv6",
                    ],
                );
            } else if owner.ruleset_version.is_none()
                && legacy_redirect_matches(&device, &owner.ifb_name)
            {
                let pref = LEGACY_REDIRECT_PREF.to_string();
                let _ = run_ok(
                    "tc",
                    &[
                        "filter", "del", "dev", &device, "ingress", "pref", &pref, "protocol",
                        "all",
                    ],
                );
            }

            let root_is_owned = owner.interface == device
                && root_qdisc_line(&device)
                    .map(|line| line.starts_with("qdisc htb 1: root"))
                    .unwrap_or(false);
            if root_is_owned {
                let _ = run_ok("tc", &["qdisc", "del", "dev", &device, "root"]);
            }

            if owner.created_clsact
                && filters_empty(&device, "ingress")
                && filters_empty(&device, "egress")
            {
                let _ = run_ok("tc", &["qdisc", "del", "dev", &device, "clsact"]);
            }
        }
    }

    if !owner.ifb_name.is_empty() && link_exists(&owner.ifb_name) {
        let _ = run_ok("tc", &["qdisc", "del", "dev", &owner.ifb_name, "root"]);
        let _ = run_ok("ip", &["link", "set", "dev", &owner.ifb_name, "down"]);
        let _ = run_ok("ip", &["link", "delete", &owner.ifb_name, "type", "ifb"]);
    }

    remove_state_files(&config.state_dir);
    Ok(())
}

pub fn detect_interface(config: &Config) -> Result<String> {
    if config.interface != "auto" {
        if !link_exists(&config.interface) {
            bail!("网卡不存在：{}", config.interface);
        }
        return Ok(config.interface.clone());
    }

    for family in ["-4", "-6"] {
        let Ok(output) = run_capture("ip", &[family, "route", "show", "default"]) else {
            continue;
        };
        for line in output.lines() {
            if !line.starts_with("default") {
                continue;
            }
            let mut tokens = line.split_whitespace();
            while let Some(token) = tokens.next() {
                if token == "dev" {
                    if let Some(device) = tokens.next() {
                        return Ok(device.to_string());
                    }
                }
            }
        }
    }
    bail!("无法识别默认网卡，请手动指定：iplimit start <Mbps> <网卡>")
}

pub fn local_addresses(device: &str) -> Result<HashSet<IpAddr>> {
    let output = run_capture(
        "ip",
        &["-o", "addr", "show", "dev", device, "scope", "global"],
    )?;
    let mut addresses = HashSet::new();
    for line in output.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(position) = tokens
            .iter()
            .position(|token| *token == "inet" || *token == "inet6")
        else {
            continue;
        };
        let Some(raw) = tokens.get(position + 1) else {
            continue;
        };
        let address = raw.split('/').next().unwrap_or(raw);
        if let Ok(parsed) = address.parse::<IpAddr>() {
            addresses.insert(parsed);
        }
    }
    Ok(addresses)
}

pub fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("此命令需要 root 权限，请使用 sudo 运行");
    }
    Ok(())
}

pub fn require_commands(names: &[&str]) -> Result<()> {
    for name in names {
        if !command_exists(name) {
            bail!("缺少依赖：{name}（请重新运行 install.sh）");
        }
    }
    Ok(())
}

pub fn command_exists(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return true;
        }
    }
    false
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && (metadata.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

pub fn load_module(name: &str) {
    let _ = run_ok("modprobe", &[name]);
}

fn link_exists(name: &str) -> bool {
    run_ok("ip", &["link", "show", "dev", name])
}

fn qdisc_show(device: &str) -> Option<String> {
    run_capture_ok("tc", &["qdisc", "show", "dev", device])
}

fn root_qdisc_line(device: &str) -> Option<String> {
    qdisc_show(device)?
        .lines()
        .find(|line| line.contains(" root "))
        .map(str::to_string)
}

fn parse_qdisc(line: &str) -> (String, String) {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "qdisc" {
            let kind = tokens.next().unwrap_or("").to_string();
            let handle = tokens.next().unwrap_or("").to_string();
            return (kind, handle);
        }
    }
    (String::new(), String::new())
}

fn assert_safe_root_qdisc(device: &str, owner: Option<&Owner>) -> Result<()> {
    let Some(line) = root_qdisc_line(device) else {
        return Ok(());
    };
    if root_qdisc_is_safe(&line, owner, device) {
        return Ok(());
    }
    let (kind, _) = parse_qdisc(&line);
    bail!("拒绝替换 {device} 上已有的根队列 '{kind}'，请先移除或迁移现有 tc 配置")
}

fn root_qdisc_is_safe(line: &str, owner: Option<&Owner>, device: &str) -> bool {
    let (kind, handle) = parse_qdisc(line);
    match kind.as_str() {
        "" => true,
        "noqueue" | "mq" | "fq" | "fq_codel" | "pfifo_fast" => handle == "0:",
        "htb" => {
            owner
                .map(|value| value.interface.as_str() == device)
                .unwrap_or(false)
                && line.starts_with("qdisc htb 1: root")
        }
        _ => false,
    }
}

fn priority_has_filters(device: &str, pref: u16) -> bool {
    let pref = pref.to_string();
    run_capture_ok(
        "tc",
        &["filter", "show", "dev", device, "ingress", "pref", &pref],
    )
    .map(|text| !text.trim().is_empty())
    .unwrap_or(false)
}

fn filters_empty(device: &str, direction: &str) -> bool {
    run_capture_ok("tc", &["filter", "show", "dev", device, direction])
        .map(|text| text.trim().is_empty())
        .unwrap_or(true)
}

fn legacy_redirect_matches(device: &str, expected_ifb: &str) -> bool {
    let pref = LEGACY_REDIRECT_PREF.to_string();
    let Some(text) = run_capture_ok(
        "tc",
        &["filter", "show", "dev", device, "ingress", "pref", &pref],
    ) else {
        return false;
    };
    text.contains("matchall")
        && text.contains(&format!(
            "mirred (Egress Redirect to device {expected_ifb})"
        ))
}

fn run_checked(program: &str, args: &[&str]) -> Result<()> {
    let output = run_output(program, args)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("{program} {} 失败：{}", args.join(" "), stderr.trim())
}

fn run_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run_capture(program: &str, args: &[&str]) -> Result<String> {
    let output = run_output(program, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {} 失败：{}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_capture_ok(program: &str, args: &[&str]) -> Option<String> {
    let output: Output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_output(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("执行 {program} 失败：{error}").into())
}

fn run_command_spec(command: &CommandSpec) -> Result<()> {
    let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
    run_checked(command.program, &args)
}

fn run_command_spec_ok(command: &CommandSpec) -> bool {
    let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
    run_ok(command.program, &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn config() -> Config {
        let mut config = Config::defaults();
        config.rate_mbit = "20".to_string();
        config.ifb_name = "ifb-iplimit".to_string();
        config.unshaped_rate_mbit = "100000".to_string();
        config
    }

    #[test]
    fn builds_the_same_filter_identity_as_the_shell_tool() {
        let v4 = client_spec(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)), 10);
        assert_eq!(v4.protocol, "ip");
        assert_eq!(v4.filter_priority, 49150);
        assert_eq!(v4.handle, "0xa");
        assert_eq!(v4.class_hex, "a");
        assert_eq!(v4.download_cidr, "198.51.100.7/32");
        assert_eq!(v4.upload_cidr, "198.51.100.7/32");

        let v6 = client_spec("2001:db8::7".parse::<IpAddr>().unwrap(), 11);
        assert_eq!(v6.protocol, "ipv6");
        assert_eq!(v6.filter_priority, 49151);
        assert_eq!(v6.handle, "0xb");
        assert_eq!(v6.download_cidr, "2001:db8::7/128");
    }

    #[test]
    fn builds_the_same_add_order_and_arguments_as_the_shell_tool() {
        let config = config();
        let spec = client_spec("198.51.100.7".parse().unwrap(), 10);
        let commands = add_client_commands(&config, "eth0", &spec);
        assert_eq!(commands.len(), 7);

        assert_eq!(commands[0].program, "tc");
        assert_eq!(
            commands[0].args,
            vec![
                "class", "add", "dev", "eth0", "parent", "1:", "classid", "1:a", "htb", "rate",
                "20mbit", "ceil", "20mbit", "quantum", "1514"
            ]
        );
        assert_eq!(
            commands[1].args,
            vec!["qdisc", "add", "dev", "eth0", "parent", "1:a", "handle", "a:", "fq_codel"]
        );
        assert_eq!(
            commands[2].args,
            vec![
                "filter",
                "add",
                "dev",
                "eth0",
                "parent",
                "1:",
                "pref",
                "49150",
                "protocol",
                "ip",
                "handle",
                "0xa",
                "flower",
                "dst_ip",
                "198.51.100.7/32",
                "classid",
                "1:a"
            ]
        );
        assert_eq!(
            commands[3].args,
            vec![
                "class",
                "add",
                "dev",
                "ifb-iplimit",
                "parent",
                "1:",
                "classid",
                "1:a",
                "htb",
                "rate",
                "20mbit",
                "ceil",
                "20mbit",
                "quantum",
                "1514"
            ]
        );
        assert_eq!(
            commands[4].args,
            vec![
                "qdisc",
                "add",
                "dev",
                "ifb-iplimit",
                "parent",
                "1:a",
                "handle",
                "a:",
                "fq_codel"
            ]
        );
        assert_eq!(
            commands[5].args,
            vec![
                "filter",
                "add",
                "dev",
                "ifb-iplimit",
                "parent",
                "1:",
                "pref",
                "49150",
                "protocol",
                "ip",
                "handle",
                "0xa",
                "flower",
                "src_ip",
                "198.51.100.7/32",
                "classid",
                "1:a"
            ]
        );
        assert_eq!(
            commands[6].args,
            vec![
                "filter",
                "add",
                "dev",
                "eth0",
                "ingress",
                "pref",
                "49150",
                "protocol",
                "ip",
                "handle",
                "0xa",
                "flower",
                "src_ip",
                "198.51.100.7/32",
                "action",
                "mirred",
                "egress",
                "redirect",
                "dev",
                "ifb-iplimit"
            ]
        );
    }

    #[test]
    fn builds_the_same_remove_order_as_the_shell_tool() {
        let config = config();
        let commands = remove_client_commands(&config, "eth0", "198.51.100.7".parse().unwrap(), 10);
        assert_eq!(commands.len(), 7);
        assert_eq!(
            commands[0].args,
            vec![
                "filter", "del", "dev", "eth0", "ingress", "pref", "49150", "protocol", "ip",
                "handle", "0xa", "flower"
            ]
        );
        assert_eq!(
            commands[1].args,
            vec![
                "filter", "del", "dev", "eth0", "parent", "1:", "pref", "49150", "protocol", "ip",
                "handle", "0xa", "flower"
            ]
        );
        assert_eq!(
            commands[2].args,
            vec!["qdisc", "del", "dev", "eth0", "parent", "1:a"]
        );
        assert_eq!(
            commands[3].args,
            vec!["class", "del", "dev", "eth0", "classid", "1:a"]
        );
        assert_eq!(
            commands[5].args,
            vec!["qdisc", "del", "dev", "ifb-iplimit", "parent", "1:a"]
        );
        assert_eq!(
            commands[6].args,
            vec!["class", "del", "dev", "ifb-iplimit", "classid", "1:a"]
        );
    }

    #[test]
    fn root_qdisc_safety_matches_the_shell_tool() {
        let owner = Owner {
            interface: "eth0".to_string(),
            ifb_name: "ifb-iplimit".to_string(),
            created_clsact: false,
            ruleset_version: Some("2".to_string()),
        };
        assert!(root_qdisc_is_safe("qdisc mq 0: root", None, "eth0"));
        assert!(root_qdisc_is_safe("qdisc noqueue 0: root", None, "eth0"));
        assert!(!root_qdisc_is_safe("qdisc cake 8001: root", None, "eth0"));
        assert!(root_qdisc_is_safe(
            "qdisc htb 1: root refcnt 2",
            Some(&owner),
            "eth0"
        ));
        assert!(!root_qdisc_is_safe(
            "qdisc htb 1: root refcnt 2",
            Some(&owner),
            "eth1"
        ));
        assert!(!root_qdisc_is_safe(
            "qdisc htb 2: root refcnt 2",
            Some(&owner),
            "eth0"
        ));
    }

    #[test]
    fn parses_qdisc_lines() {
        assert_eq!(
            parse_qdisc("qdisc htb 1: root refcnt 2"),
            ("htb".to_string(), "1:".to_string())
        );
        assert_eq!(
            parse_qdisc("qdisc clsact ffff: parent ffff:fff1"),
            ("clsact".to_string(), "ffff:".to_string())
        );
    }

    #[test]
    fn ipv6_uses_the_v6_priority_and_cidr() {
        let config = config();
        let spec = client_spec("2001:db8:2::20".parse().unwrap(), 12);
        let commands = add_client_commands(&config, "eth0", &spec);
        assert_eq!(commands[2].args[7], "49151");
        assert_eq!(commands[2].args[8], "protocol");
        assert_eq!(commands[2].args[9], "ipv6");
        assert_eq!(commands[2].args[14], "2001:db8:2::20/128");
    }

    #[test]
    fn interface_validation_uses_path_lookup() {
        assert!(command_exists("sh"));
        assert!(!command_exists("definitely-not-a-real-command-iplimit"));
    }
}
