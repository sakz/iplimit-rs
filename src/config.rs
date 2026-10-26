use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::bail;
use crate::error::Result;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/iplimit.conf";
pub const DEFAULT_STATE_DIR: &str = "/run/iplimit";

#[derive(Debug, Clone)]
pub struct Config {
    pub rate_mbit: String,
    pub interface: String,
    pub ifb_name: String,
    pub poll_interval: String,
    pub client_ttl: String,
    pub unshaped_rate_mbit: String,
    pub exclude_ips: String,
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
}

impl Config {
    pub fn defaults() -> Self {
        Self {
            rate_mbit: "20".to_string(),
            interface: "auto".to_string(),
            ifb_name: "ifb-iplimit".to_string(),
            poll_interval: "2".to_string(),
            client_ttl: "300".to_string(),
            unshaped_rate_mbit: "100000".to_string(),
            exclude_ips: String::new(),
            config_path: env_path("IPLIMIT_CONFIG_FILE", DEFAULT_CONFIG_PATH),
            state_dir: env_path("IPLIMIT_STATE_DIR", DEFAULT_STATE_DIR),
        }
    }

    pub fn load() -> Result<Self> {
        Self::load_from(
            env_path("IPLIMIT_CONFIG_FILE", DEFAULT_CONFIG_PATH),
            env_path("IPLIMIT_STATE_DIR", DEFAULT_STATE_DIR),
        )
    }

    pub fn load_from(config_path: PathBuf, state_dir: PathBuf) -> Result<Self> {
        let mut config = Self::defaults();
        config.config_path = config_path;
        config.state_dir = state_dir;
        if config.config_path.is_file() {
            let text = fs::read_to_string(&config.config_path).map_err(|error| {
                format!(
                    "读取配置文件 {} 失败：{}",
                    config.config_path.display(),
                    error
                )
            })?;
            config.apply_text(&text)?;
        }
        Ok(config)
    }

    pub fn apply_text(&mut self, text: &str) -> Result<()> {
        for (index, raw_line) in text.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let Some((raw_key, raw_value)) = line.split_once('=') else {
                bail!(
                    "配置文件 {} 第 {} 行格式无效：{}",
                    self.config_path.display(),
                    index + 1,
                    raw_line
                );
            };
            let key = raw_key.trim();
            let value = unquote(raw_value.trim());
            match key {
                "RATE_MBIT" => self.rate_mbit = value,
                "INTERFACE" => self.interface = value,
                "IFB_NAME" => self.ifb_name = value,
                "POLL_INTERVAL" => self.poll_interval = value,
                "CLIENT_TTL" => self.client_ttl = value,
                "UNSHAPED_RATE_MBIT" => self.unshaped_rate_mbit = value,
                "EXCLUDE_IPS" => self.exclude_ips = value,
                _ => {}
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if !is_valid_rate(&self.rate_mbit) {
            bail!("RATE_MBIT 必须是正数，当前值：{}", self.rate_mbit);
        }
        if !is_positive_integer(&self.poll_interval) {
            bail!("POLL_INTERVAL 必须是正整数，当前值：{}", self.poll_interval);
        }
        if !is_positive_integer(&self.client_ttl) {
            bail!("CLIENT_TTL 必须是正整数，当前值：{}", self.client_ttl);
        }
        if !is_valid_rate(&self.unshaped_rate_mbit) {
            bail!(
                "UNSHAPED_RATE_MBIT 必须是正数，当前值：{}",
                self.unshaped_rate_mbit
            );
        }
        if self.interface != "auto" && !is_valid_interface_name(&self.interface) {
            bail!("网卡名称无效（最长 15 个字符）：{}", self.interface);
        }
        if !is_valid_interface_name(&self.ifb_name) {
            bail!("IFB 名称无效（最长 15 个字符）：{}", self.ifb_name);
        }
        for token in self.exclude_ips.split_whitespace() {
            if IpAddr::from_str(token).is_err() {
                bail!("EXCLUDE_IPS 中的地址无效：{}", token);
            }
        }
        Ok(())
    }

    pub fn poll_interval_secs(&self) -> u64 {
        self.poll_interval.parse().unwrap_or(2)
    }

    pub fn client_ttl_secs(&self) -> u64 {
        self.client_ttl.parse().unwrap_or(300)
    }

    pub fn excluded_set(&self) -> HashSet<IpAddr> {
        self.exclude_ips
            .split_whitespace()
            .filter_map(|token| IpAddr::from_str(token).ok())
            .collect()
    }

    pub fn owner_file(&self) -> PathBuf {
        self.state_dir.join("owner.env")
    }

    pub fn clients_file(&self) -> PathBuf {
        self.state_dir.join("clients.tsv")
    }

    pub fn save(&self) -> Result<()> {
        let parent = self
            .config_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建配置目录 {} 失败：{}", parent.display(), error))?;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let temp_path = parent.join(format!(
            ".iplimit.conf.tmp.{}.{}",
            std::process::id(),
            stamp
        ));

        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp_path)
                .map_err(|error| {
                    format!("创建临时配置文件 {} 失败：{}", temp_path.display(), error)
                })?;
            let body = format!(
                "# 由 iplimit 管理。修改后请运行：sudo iplimit start\n\
                 RATE_MBIT=\"{}\"\n\
                 INTERFACE=\"{}\"\n\
                 IFB_NAME=\"{}\"\n\
                 POLL_INTERVAL=\"{}\"\n\
                 CLIENT_TTL=\"{}\"\n\
                 UNSHAPED_RATE_MBIT=\"{}\"\n\
                 EXCLUDE_IPS=\"{}\"\n",
                self.rate_mbit,
                self.interface,
                self.ifb_name,
                self.poll_interval,
                self.client_ttl,
                self.unshaped_rate_mbit,
                self.exclude_ips
            );
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp_path, &self.config_path).map_err(|error| {
                format!(
                    "替换配置文件 {} 失败：{}",
                    self.config_path.display(),
                    error
                )
            })?;
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }
}

fn env_path(key: &str, default: &str) -> PathBuf {
    env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

pub fn is_valid_rate(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let mut before_dot = 0usize;
    let mut after_dot = 0usize;
    let mut seen_dot = false;
    for byte in value.bytes() {
        match byte {
            b'0'..=b'9' => {
                if seen_dot {
                    after_dot += 1;
                } else {
                    before_dot += 1;
                }
            }
            b'.' => {
                if seen_dot {
                    return false;
                }
                seen_dot = true;
            }
            _ => return false,
        }
    }
    if before_dot == 0 || (seen_dot && after_dot == 0) {
        return false;
    }
    value
        .parse::<f64>()
        .map(|parsed| parsed.is_finite() && parsed > 0.0)
        .unwrap_or(false)
}

pub fn is_positive_integer(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes[0] == b'0' || !bytes[0].is_ascii_digit() {
        return false;
    }
    if !bytes.iter().all(u8::is_ascii_digit) {
        return false;
    }
    value.parse::<u64>().is_ok()
}

pub fn is_valid_interface_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 15 {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(text: &str) -> Config {
        let mut config = Config::defaults();
        config.config_path = PathBuf::from("/tmp/test-iplimit.conf");
        config.state_dir = PathBuf::from("/tmp/test-iplimit-state");
        config.apply_text(text).unwrap();
        config
    }

    #[test]
    fn defaults_match_the_shell_tool() {
        let config = Config::defaults();
        assert_eq!(config.rate_mbit, "20");
        assert_eq!(config.interface, "auto");
        assert_eq!(config.ifb_name, "ifb-iplimit");
        assert_eq!(config.poll_interval, "2");
        assert_eq!(config.client_ttl, "300");
        assert_eq!(config.unshaped_rate_mbit, "100000");
        assert_eq!(config.exclude_ips, "");
    }

    #[test]
    fn parses_quoted_config_values_and_comments() {
        let config = config_from(
            "# comment\nRATE_MBIT=\"12.5\"\nINTERFACE='eth0'\nEXCLUDE_IPS=\"198.51.100.7 2001:db8::9\"\n",
        );
        assert_eq!(config.rate_mbit, "12.5");
        assert_eq!(config.interface, "eth0");
        assert_eq!(config.exclude_ips, "198.51.100.7 2001:db8::9");
        config.validate().unwrap();
    }

    #[test]
    fn rejects_malformed_config_lines() {
        let mut config = Config::defaults();
        config.config_path = PathBuf::from("/tmp/test-iplimit.conf");
        let error = config.apply_text("RATE_MBIT 20\n").unwrap_err();
        assert!(error.to_string().contains("格式无效"));
    }

    #[test]
    fn rate_validation_matches_the_shell_tool() {
        assert!(is_valid_rate("20"));
        assert!(is_valid_rate("2.5"));
        assert!(is_valid_rate("0.1"));
        assert!(!is_valid_rate("0"));
        assert!(!is_valid_rate("0.0"));
        assert!(!is_valid_rate("-1"));
        assert!(!is_valid_rate("1Mbps"));
        assert!(!is_valid_rate(".5"));
        assert!(!is_valid_rate("1."));
        assert!(!is_valid_rate("1.2.3"));
    }

    #[test]
    fn positive_integer_validation_matches_the_shell_tool() {
        assert!(is_positive_integer("1"));
        assert!(is_positive_integer("300"));
        assert!(!is_positive_integer("0"));
        assert!(!is_positive_integer("02"));
        assert!(!is_positive_integer("-2"));
        assert!(!is_positive_integer("2.5"));
        assert!(!is_positive_integer("99999999999999999999999999"));
    }

    #[test]
    fn interface_name_validation_matches_the_shell_tool() {
        assert!(is_valid_interface_name("eth0"));
        assert!(is_valid_interface_name("ifb-iplimit"));
        assert!(is_valid_interface_name("wan.100"));
        assert!(!is_valid_interface_name(""));
        assert!(!is_valid_interface_name("ifb-iplimit-test"));
        assert!(!is_valid_interface_name("eth 0"));
    }

    #[test]
    fn validates_exclude_ips() {
        let config = config_from("EXCLUDE_IPS=\"203.0.113.9 2001:db8::10\"\n");
        config.validate().unwrap();
        let config = config_from("EXCLUDE_IPS=\"203.0.113.999\"\n");
        assert!(config.validate().is_err());
    }

    #[test]
    fn parses_negative_and_invalid_numbers() {
        let config = config_from("POLL_INTERVAL=\"0\"\nCLIENT_TTL=\"abc\"\n");
        assert!(config.validate().is_err());
    }
}
