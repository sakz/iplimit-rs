use std::fs;
use std::io::{self, Write};
use std::process::{Command, Output};

use crate::bail;
use crate::config::Config;
use crate::daemon;
use crate::error::Result;
use crate::log::{log, warn};
use crate::network::{
    cleanup_network, command_exists, detect_interface, require_commands, require_root,
};
use crate::state::format_clients;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const SERVICE_NAME: &str = "iplimit.service";

pub fn run(args: Vec<String>) -> Result<()> {
    let Some(command) = args.first() else {
        return interactive_menu();
    };
    let rest = &args[1..];
    match command.as_str() {
        "start" => start_limit(rest),
        "set" => set_limit(rest),
        "status" => {
            ensure_no_args(rest, "status")?;
            show_status()
        }
        "clients" => {
            ensure_no_args(rest, "clients")?;
            show_clients()
        }
        "stop" | "clean" => {
            ensure_no_args(rest, "stop")?;
            stop_limit()
        }
        "help" | "-h" | "--help" => {
            usage();
            Ok(())
        }
        "version" | "-V" | "--version" => {
            println!("iplimit {VERSION}");
            Ok(())
        }
        "daemon-internal" => {
            ensure_no_args(rest, "daemon-internal")?;
            daemon::run(Config::load()?)
        }
        "cleanup-internal" => {
            ensure_no_args(rest, "cleanup-internal")?;
            require_root()?;
            cleanup_network(&load_config_for_cleanup(), None)
        }
        other => bail!("未知命令：{other}（请运行 iplimit help 查看帮助）"),
    }
}

fn usage() {
    println!(
        "IPLimit - Debian 12 按客户端 IP 独立限速工具\n\
         \n\
         用法：\n\
         \x20 iplimit                         打开交互式菜单\n\
         \x20 iplimit start [Mbps] [网卡]      启动或更新限速（默认：20、自动识别网卡）\n\
         \x20 iplimit set <Mbps>              修改每个客户端的限速并重启服务\n\
         \x20 iplimit status                  查看服务、队列和配置状态\n\
         \x20 iplimit clients                 查看已发现的客户端 IP 和类别 ID\n\
         \x20 iplimit stop                    停止限速并清理本工具创建的 tc/IFB 配置\n\
         \x20 iplimit help                    显示帮助\n\
         \x20 iplimit version                 显示版本\n\
         \n\
         示例：\n\
         \x20 sudo iplimit start 20\n\
         \x20 sudo iplimit start 12.5 eth0\n\
         \x20 sudo iplimit set 50\n\
         \x20 sudo iplimit status\n\
         \n\
         每个直连客户端 IP 都有独立的上传和下载上限，支持 TCP、UDP、IPv4 和 IPv6。\n\
         未识别的流量不会进入 IFB，也不会被限速。"
    );
}

fn ensure_no_args(args: &[String], command: &str) -> Result<()> {
    if args.is_empty() {
        Ok(())
    } else {
        bail!("用法：iplimit {command}")
    }
}

fn start_limit(args: &[String]) -> Result<()> {
    require_root()?;
    require_commands(&["ip", "tc"])?;
    require_systemctl()?;

    let mut config = Config::load()?;
    if let Some(rate) = args.first() {
        if !rate.is_empty() {
            config.rate_mbit = rate.clone();
        }
    }
    if let Some(interface) = args.get(1) {
        if !interface.is_empty() {
            config.interface = interface.clone();
        }
    }
    if args.len() > 2 {
        bail!("参数过多。用法：iplimit start [Mbps] [网卡]");
    }
    config.validate()?;
    config.save()?;

    systemctl_checked(&["daemon-reload"])?;
    systemctl_checked(&["enable", "--quiet", SERVICE_NAME])?;
    systemctl_checked(&["restart", SERVICE_NAME])?;
    if !systemctl_ok(&["is-active", "--quiet", SERVICE_NAME]) {
        bail!("服务启动失败，请检查：journalctl -u {SERVICE_NAME} -n 100 --no-pager");
    }
    log(format!(
        "已启动：每个客户端 {} Mbps（网卡：{}）",
        config.rate_mbit, config.interface
    ));
    Ok(())
}

fn set_limit(args: &[String]) -> Result<()> {
    if args.len() != 1 || args[0].is_empty() {
        bail!("用法：iplimit set <Mbps>");
    }
    start_limit(args)
}

fn stop_limit() -> Result<()> {
    require_root()?;
    let config = load_config_for_cleanup();
    let _ = systemctl_ok(&["disable", "--now", SERVICE_NAME]);
    cleanup_network(&config, None)?;
    log("已停止，并清理 IPLimit 创建的 tc 规则和 IFB 设备");
    Ok(())
}

fn show_clients() -> Result<()> {
    require_root()?;
    let config = load_config_for_cleanup();
    print_clients(&config);
    Ok(())
}

fn show_status() -> Result<()> {
    require_root()?;
    let config = Config::load()?;
    config.validate()?;
    let device = detect_interface(&config)?;

    println!(
        "服务状态：{}",
        if systemctl_ok(&["is-active", "--quiet", SERVICE_NAME]) {
            "运行中"
        } else {
            "未运行"
        }
    );
    println!(
        "开机启动：{}",
        if systemctl_ok(&["is-enabled", "--quiet", SERVICE_NAME]) {
            "已启用"
        } else {
            "未启用"
        }
    );
    println!("公网网卡：{device}");
    println!("IFB 设备：{}", config.ifb_name);
    println!("限速配置：每个客户端每个方向 {} Mbps", config.rate_mbit);
    println!("支持协议：IPv4/IPv6、TCP/UDP");
    println!("空闲清理：{} 秒", config.client_ttl);

    println!("\n{device} 上的根队列：");
    print_command_output("tc", &["qdisc", "show", "dev", &device]);
    if link_exists(&config.ifb_name) {
        println!("\n{} 上的根队列：", config.ifb_name);
        print_command_output("tc", &["qdisc", "show", "dev", &config.ifb_name]);
    }

    println!("\n客户端：");
    print_clients(&config);
    Ok(())
}

fn print_clients(config: &Config) {
    let path = config.clients_file();
    if !path.is_file() {
        log("暂无客户端记录（服务可能未运行，或尚未完成首次扫描）");
        return;
    }
    match fs::read_to_string(&path) {
        Ok(text) => print!("{}", format_clients(&text)),
        Err(error) => warn(format!("读取客户端记录 {} 失败：{error}", path.display())),
    }
}

fn interactive_menu() -> Result<()> {
    require_root()?;
    loop {
        println!(
            "\nIPLimit\n  \
             1) 启动或修改每个客户端的限速\n  \
             2) 查看状态\n  \
             3) 查看活跃客户端\n  \
             4) 停止并清理\n  \
             5) 退出"
        );
        let Some(choice) = read_line("请选择 [1-5]：") else {
            return Ok(());
        };
        match choice.as_str() {
            "1" => {
                let Some(rate) = read_line_default("每个客户端的限速 Mbps [20]：", "20")
                else {
                    return Ok(());
                };
                let Some(interface) = read_line_default("公网网卡 [auto]：", "auto") else {
                    return Ok(());
                };
                start_limit(&[rate, interface])?;
            }
            "2" => show_status()?,
            "3" => show_clients()?,
            "4" => stop_limit()?,
            "5" => return Ok(()),
            _ => warn("请输入 1 到 5 之间的数字"),
        }
    }
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

fn read_line_default(prompt: &str, default: &str) -> Option<String> {
    let value = read_line(prompt)?;
    if value.is_empty() {
        Some(default.to_string())
    } else {
        Some(value)
    }
}

fn require_systemctl() -> Result<()> {
    if command_exists("systemctl") {
        Ok(())
    } else {
        bail!("缺少依赖：systemctl（请重新运行 install.sh）")
    }
}

fn systemctl_checked(args: &[&str]) -> Result<()> {
    let output = run_systemctl(args)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        bail!("systemctl {} 失败", args.join(" "));
    }
    bail!("systemctl {} 失败：{detail}", args.join(" "));
}

fn systemctl_ok(args: &[&str]) -> bool {
    run_systemctl(args)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run_systemctl(args: &[&str]) -> Result<Output> {
    Command::new("systemctl")
        .args(args)
        .output()
        .map_err(|error| format!("执行 systemctl 失败：{error}").into())
}

fn load_config_for_cleanup() -> Config {
    match Config::load() {
        Ok(config) => config,
        Err(error) => {
            warn(format!("读取配置失败，使用默认路径：{error}"));
            Config::defaults()
        }
    }
}

fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", "dev", name])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn print_command_output(program: &str, args: &[&str]) {
    if let Ok(output) = Command::new(program).args(args).output() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
}
