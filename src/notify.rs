use std::env;
use std::ffi::OsStr;
use std::io;
use std::mem;
use std::os::unix::ffi::OsStrExt;

use crate::bail;
use crate::error::Result;

pub fn notify(message: &str) -> Result<()> {
    let Some(socket) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    send(socket.as_os_str(), message.as_bytes())
}

fn send(socket: &OsStr, message: &[u8]) -> Result<()> {
    let name = socket.as_bytes();
    if name.is_empty() {
        bail!("NOTIFY_SOCKET 为空");
    }

    let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let capacity = address.sun_path.len();
    let length = if name[0] == b'@' {
        let abstract_name = &name[1..];
        if abstract_name.len() + 1 > capacity {
            bail!("NOTIFY_SOCKET 路径过长");
        }
        address.sun_path[0] = 0;
        for (index, byte) in abstract_name.iter().enumerate() {
            address.sun_path[index + 1] = *byte as libc::c_char;
        }
        1 + abstract_name.len()
    } else {
        if name.len() + 1 > capacity {
            bail!("NOTIFY_SOCKET 路径过长");
        }
        for (index, byte) in name.iter().enumerate() {
            address.sun_path[index] = *byte as libc::c_char;
        }
        name.len() + 1
    };
    let path_offset = (&address.sun_path as *const _ as usize) - (&address as *const _ as usize);
    let address_length = path_offset + length;

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        bail!(
            "创建 systemd 通知套接字失败：{}",
            io::Error::last_os_error()
        );
    }
    let sent = unsafe {
        libc::sendto(
            fd,
            message.as_ptr() as *const libc::c_void,
            message.len(),
            0,
            &address as *const libc::sockaddr_un as *const libc::sockaddr,
            address_length as libc::socklen_t,
        )
    };
    let error = if sent < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };
    unsafe {
        libc::close(fd);
    }
    if let Some(error) = error {
        bail!("发送 systemd 通知失败：{error}");
    }
    Ok(())
}

pub fn ready(status: &str) -> Result<()> {
    notify(&format!("READY=1\nSTATUS={status}"))
}

pub fn stopping(status: &str) -> Result<()> {
    notify(&format!("STOPPING=1\nSTATUS={status}"))
}
