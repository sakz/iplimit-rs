use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::error::Result;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owner {
    pub interface: String,
    pub ifb_name: String,
    pub created_clsact: bool,
    pub ruleset_version: Option<String>,
}

pub fn read_owner(path: &Path) -> Option<Owner> {
    if !path.is_file() {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let mut owner = Owner::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "INTERFACE" => owner.interface = value.trim().to_string(),
            "IFB_NAME" => owner.ifb_name = value.trim().to_string(),
            "CREATED_CLSACT" => owner.created_clsact = value.trim() == "1",
            "RULESET_VERSION" => owner.ruleset_version = Some(value.trim().to_string()),
            _ => {}
        }
    }
    Some(owner)
}

pub fn write_owner(
    path: &Path,
    interface: &str,
    ifb_name: &str,
    created_clsact: bool,
) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("创建状态目录 {} 失败：{}", parent.display(), error))?;

    let temp_path = temp_sibling(path, "owner");
    let body = format!(
        "INTERFACE={interface}\nIFB_NAME={ifb_name}\nCREATED_CLSACT={}\nRULESET_VERSION=2\n",
        u8::from(created_clsact)
    );
    write_atomic(path, &temp_path, body.as_bytes(), 0o644)
}

pub fn write_clients(path: &Path, rows: &[(String, u16, u64)]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("创建状态目录 {} 失败：{}", parent.display(), error))?;

    let temp_path = temp_sibling(path, "clients");
    let mut body = String::from("IP\t类别\t最后活跃时间戳\n");
    let mut sorted: Vec<&(String, u16, u64)> = rows.iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    for (ip, class_id, last_seen) in sorted {
        body.push_str(&format!("{ip}\t1:{class_id:x}\t{last_seen}\n"));
    }
    write_atomic(path, &temp_path, body.as_bytes(), 0o644)
}

pub fn format_clients(text: &str) -> String {
    let rows: Vec<Vec<&str>> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split('\t').collect())
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in &rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(value.chars().count());
        }
    }
    let mut output = String::new();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            if index + 1 == row.len() {
                output.push_str(value);
            } else {
                let padding = widths[index].saturating_sub(value.chars().count()) + 2;
                output.push_str(value);
                output.push_str(&" ".repeat(padding));
            }
        }
        output.push('\n');
    }
    output
}

pub fn remove_state_files(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join("owner.env"));
    let _ = fs::remove_file(state_dir.join("clients.tsv"));
    let _ = fs::remove_dir(state_dir);
}

fn temp_sibling(path: &Path, label: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let name = format!(".{label}.tmp.{}.{}", std::process::id(), stamp);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => std::path::PathBuf::from(name),
    }
}

fn write_atomic(path: &Path, temp_path: &Path, body: &[u8], mode: u32) -> Result<()> {
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(temp_path)
            .map_err(|error| format!("创建临时状态文件 {} 失败：{}", temp_path.display(), error))?;
        file.write_all(body)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temp_path, path)
            .map_err(|error| format!("替换状态文件 {} 失败：{}", path.display(), error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_owner_files() {
        let dir = std::env::temp_dir().join(format!("iplimit-owner-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owner.env");
        fs::write(
            &path,
            "INTERFACE=eth0\nIFB_NAME=ifb-iplimit\nCREATED_CLSACT=1\nRULESET_VERSION=2\n",
        )
        .unwrap();
        let owner = read_owner(&path).unwrap();
        assert_eq!(owner.interface, "eth0");
        assert_eq!(owner.ifb_name, "ifb-iplimit");
        assert!(owner.created_clsact);
        assert_eq!(owner.ruleset_version.as_deref(), Some("2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_and_reads_owner_files() {
        let dir = std::env::temp_dir().join(format!("iplimit-owner-rw-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owner.env");
        write_owner(&path, "eth0", "ifb-test", true).unwrap();
        let owner = read_owner(&path).unwrap();
        assert_eq!(owner.interface, "eth0");
        assert_eq!(owner.ifb_name, "ifb-test");
        assert!(owner.created_clsact);
        assert_eq!(owner.ruleset_version.as_deref(), Some("2"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn formats_clients_in_sorted_tsv_order() {
        let rows = vec![
            ("2001:db8::9".to_string(), 12u16, 100u64),
            ("198.51.100.7".to_string(), 10u16, 90u64),
        ];
        let dir = std::env::temp_dir().join(format!("iplimit-clients-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clients.tsv");
        write_clients(&path, &rows).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("IP\t类别\t最后活跃时间戳\n"));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "198.51.100.7\t1:a\t90");
        assert_eq!(lines[2], "2001:db8::9\t1:c\t100");
        let formatted = format_clients(&text);
        assert!(formatted.contains("198.51.100.7"));
        assert!(formatted.contains("2001:db8::9"));
        let _ = fs::remove_dir_all(&dir);
    }
}
