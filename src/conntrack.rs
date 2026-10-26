use std::collections::HashMap;
use std::io;
use std::mem;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use crate::bail;
use crate::error::{Error, Result};

const NFNL_SUBSYS_CTNETLINK: u16 = 1;
const IPCTNL_MSG_CT_NEW: u8 = 0;
const IPCTNL_MSG_CT_GET: u8 = 1;
const IPCTNL_MSG_CT_DELETE: u8 = 2;

const NFNLGRP_CONNTRACK_NEW: u32 = 1;
const NFNLGRP_CONNTRACK_UPDATE: u32 = 2;
const NFNLGRP_CONNTRACK_DESTROY: u32 = 3;

const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_DUMP_INTR: u16 = 0x10;
const NLM_F_ROOT: u16 = 0x100;
const NLM_F_MATCH: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLMSG_OVERRUN: u16 = 4;

const CTA_TUPLE_ORIG: u16 = 1;
const CTA_TUPLE_IP: u16 = 1;
const CTA_TUPLE_PROTO: u16 = 2;
const CTA_PROTO_NUM: u16 = 1;
const CTA_IP_V4_SRC: u16 = 1;
const CTA_IP_V4_DST: u16 = 2;
const CTA_IP_V6_SRC: u16 = 3;
const CTA_IP_V6_DST: u16 = 4;

const NLMSG_HEADER_LEN: usize = 16;
const NFGENMSG_LEN: usize = 4;
const NLA_HEADER_LEN: usize = 4;
const NLA_TYPE_MASK: u16 = 0x3fff;

const DUMP_TIMEOUT: Duration = Duration::from_secs(5);
const DUMP_RETRIES: usize = 3;
// dump 期间同一套接字上仍会收到多播事件；给积压队列设上限，
// 防止大表 + 高事件率的机器上无界增长。
const MAX_QUEUED_EVENTS: usize = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtTuple {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub protocol: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtEvent {
    New,
    Update,
    Destroy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CtMessage {
    Entry(CtTuple),
    Event { kind: CtEvent, tuple: CtTuple },
    Done { interrupted: bool },
    Error(i32),
    Overrun,
}

pub struct DumpResult {
    pub counts: HashMap<IpAddr, u32>,
    pub events: Vec<CtMessage>,
}

pub struct ConntrackSocket {
    fd: RawFd,
    sequence: u32,
    receive_buffer: Vec<u8>,
}

impl ConntrackSocket {
    pub fn open() -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::NETLINK_NETFILTER,
            )
        };
        if fd < 0 {
            bail!("创建 ctnetlink 套接字失败：{}", io::Error::last_os_error());
        }
        let socket = Self {
            fd,
            sequence: 1,
            receive_buffer: vec![0u8; 256 * 1024],
        };

        let mut address: libc::sockaddr_nl = unsafe { mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        let bind_result = unsafe {
            libc::bind(
                socket.fd,
                &address as *const libc::sockaddr_nl as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bind_result < 0 {
            bail!("绑定 ctnetlink 套接字失败：{}", io::Error::last_os_error());
        }

        for group in [
            NFNLGRP_CONNTRACK_NEW,
            NFNLGRP_CONNTRACK_UPDATE,
            NFNLGRP_CONNTRACK_DESTROY,
        ] {
            let option = group;
            let result = unsafe {
                libc::setsockopt(
                    socket.fd,
                    libc::SOL_NETLINK,
                    libc::NETLINK_ADD_MEMBERSHIP,
                    &option as *const u32 as *const libc::c_void,
                    mem::size_of::<u32>() as libc::socklen_t,
                )
            };
            if result < 0 {
                bail!(
                    "订阅 conntrack 事件组 {group} 失败：{}",
                    io::Error::last_os_error()
                );
            }
        }

        let receive_buffer_size: i32 = 8 * 1024 * 1024;
        // SO_RCVBUF 会被内核静默截断到 net.core.rmem_max（默认约 208 KB），
        // 高事件率下毫秒级溢出；root 守护进程改用 RCVBUFFORCE 拿到真实
        // 的大缓冲，失败（无 CAP_NET_ADMIN）再退回普通 SO_RCVBUF。
        let forced = unsafe {
            libc::setsockopt(
                socket.fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUFFORCE,
                &receive_buffer_size as *const i32 as *const libc::c_void,
                mem::size_of::<i32>() as libc::socklen_t,
            )
        };
        if forced < 0 {
            unsafe {
                libc::setsockopt(
                    socket.fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    &receive_buffer_size as *const i32 as *const libc::c_void,
                    mem::size_of::<i32>() as libc::socklen_t,
                );
            }
        }

        Ok(socket)
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn send_dump(&mut self) -> Result<u32> {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1).max(1);

        let mut message = [0u8; NLMSG_HEADER_LEN + NFGENMSG_LEN];
        let message_length = message.len() as u32;
        write_u32(&mut message, 0, message_length);
        write_u16(
            &mut message,
            4,
            (NFNL_SUBSYS_CTNETLINK << 8) | u16::from(IPCTNL_MSG_CT_GET),
        );
        write_u16(&mut message, 6, NLM_F_REQUEST | NLM_F_DUMP);
        write_u32(&mut message, 8, sequence);
        write_u32(&mut message, 12, 0);
        message[16] = libc::AF_UNSPEC as u8;
        message[17] = 0;
        write_u16(&mut message, 18, 0);

        let mut address: libc::sockaddr_nl = unsafe { mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        let sent = unsafe {
            libc::sendto(
                self.fd,
                message.as_ptr() as *const libc::c_void,
                message.len(),
                0,
                &address as *const libc::sockaddr_nl as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            bail!(
                "发送 ctnetlink dump 请求失败：{}",
                io::Error::last_os_error()
            );
        }
        Ok(sequence)
    }

    pub fn receive(&mut self) -> io::Result<&[u8]> {
        let received = unsafe {
            libc::recv(
                self.fd,
                self.receive_buffer.as_mut_ptr() as *mut libc::c_void,
                self.receive_buffer.len(),
                0,
            )
        };
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(&self.receive_buffer[..received as usize])
    }

    pub fn try_receive(&mut self) -> io::Result<Option<&[u8]>> {
        match self.receive() {
            Ok(buffer) => Ok(Some(buffer)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut poll_fd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
        let result = unsafe { libc::poll(&mut poll_fd, 1, milliseconds) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result > 0)
    }

    pub fn blocking_dump(
        &mut self,
        mut accept: impl FnMut(&CtTuple) -> bool,
    ) -> Result<DumpResult> {
        let mut queued_events = Vec::new();
        for _ in 0..DUMP_RETRIES {
            match self.dump_once(&mut accept, &mut queued_events, DUMP_TIMEOUT) {
                Ok(counts) => {
                    return Ok(DumpResult {
                        counts,
                        events: queued_events,
                    })
                }
                Err(DumpFailure::Interrupted) => continue,
                Err(DumpFailure::Io(error)) => return Err(error),
            }
        }
        bail!("ctnetlink dump 连续被中断，无法取得完整的客户端列表")
    }

    fn dump_once(
        &mut self,
        accept: &mut impl FnMut(&CtTuple) -> bool,
        queued_events: &mut Vec<CtMessage>,
        timeout: Duration,
    ) -> std::result::Result<HashMap<IpAddr, u32>, DumpFailure> {
        let sequence = self.send_dump().map_err(DumpFailure::Io)?;
        let deadline = Instant::now() + timeout;
        let mut counts = HashMap::new();

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(DumpFailure::Interrupted);
            }
            let readable = match self.wait_readable(deadline - now) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(DumpFailure::Io(error.into())),
            };
            if !readable {
                return Err(DumpFailure::Interrupted);
            }

            let received = match self.receive() {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                    return Err(DumpFailure::Interrupted)
                }
                Err(error) => return Err(DumpFailure::Io(error.into())),
            };

            for message in parse_datagram_filtered(received, Some(sequence)) {
                match message {
                    CtMessage::Entry(tuple) => {
                        if accept(&tuple) {
                            *counts.entry(tuple.src).or_insert(0) += 1;
                        }
                    }
                    CtMessage::Done { interrupted } => {
                        if interrupted {
                            return Err(DumpFailure::Interrupted);
                        }
                        return Ok(counts);
                    }
                    CtMessage::Event { .. } => {
                        if queued_events.len() < MAX_QUEUED_EVENTS {
                            queued_events.push(message);
                        }
                    }
                    CtMessage::Error(code) => {
                        if code == -libc::ENOBUFS || code == -libc::EINTR {
                            return Err(DumpFailure::Interrupted);
                        }
                        return Err(DumpFailure::Io(Error::new(format!(
                            "ctnetlink dump 失败：{}",
                            io::Error::from_raw_os_error(-code)
                        ))));
                    }
                    CtMessage::Overrun => return Err(DumpFailure::Interrupted),
                }
            }
        }
    }
}

impl Drop for ConntrackSocket {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

enum DumpFailure {
    Interrupted,
    Io(Error),
}

pub fn parse_datagram(buffer: &[u8]) -> Vec<CtMessage> {
    parse_datagram_filtered(buffer, None)
}

fn parse_datagram_filtered(buffer: &[u8], expected_sequence: Option<u32>) -> Vec<CtMessage> {
    let mut messages = Vec::new();
    let mut offset = 0usize;
    while offset + NLMSG_HEADER_LEN <= buffer.len() {
        let length = read_u32(buffer, offset).unwrap_or(0) as usize;
        if length < NLMSG_HEADER_LEN || offset + length > buffer.len() {
            break;
        }
        let message_type = read_u16(buffer, offset + 4).unwrap_or(0);
        let flags = read_u16(buffer, offset + 6).unwrap_or(0);
        let sequence = read_u32(buffer, offset + 8).unwrap_or(0);
        if let Some(expected) = expected_sequence {
            // 多播事件使用序号 0；忽略上一次被中断的 dump 迟到的响应。
            if sequence != expected && sequence != 0 {
                offset += align4(length);
                continue;
            }
        }
        let payload = &buffer[offset..offset + length];
        add_message(message_type, flags, payload, &mut messages);
        offset += align4(length);
    }
    messages
}

fn add_message(message_type: u16, flags: u16, payload: &[u8], output: &mut Vec<CtMessage>) {
    match message_type {
        NLMSG_DONE => output.push(CtMessage::Done {
            interrupted: flags & NLM_F_DUMP_INTR != 0,
        }),
        NLMSG_ERROR => {
            if payload.len() >= NLMSG_HEADER_LEN + 4 {
                let code = read_i32(payload, NLMSG_HEADER_LEN).unwrap_or(0);
                if code != 0 {
                    output.push(CtMessage::Error(code));
                }
            }
        }
        NLMSG_OVERRUN => output.push(CtMessage::Overrun),
        _ => {
            let subsystem = message_type >> 8;
            let operation = (message_type & 0xff) as u8;
            if subsystem != NFNL_SUBSYS_CTNETLINK || payload.len() < NLMSG_HEADER_LEN + NFGENMSG_LEN
            {
                return;
            }
            let attributes = &payload[NLMSG_HEADER_LEN + NFGENMSG_LEN..];
            let tuple = parse_original_tuple(attributes);
            match (operation, tuple) {
                (IPCTNL_MSG_CT_GET, Some(tuple)) => output.push(CtMessage::Entry(tuple)),
                (IPCTNL_MSG_CT_NEW, Some(tuple)) => {
                    let kind = if flags & NLM_F_CREATE != 0 {
                        CtEvent::New
                    } else {
                        CtEvent::Update
                    };
                    output.push(CtMessage::Event { kind, tuple });
                }
                (IPCTNL_MSG_CT_DELETE, Some(tuple)) => output.push(CtMessage::Event {
                    kind: CtEvent::Destroy,
                    tuple,
                }),
                _ => {}
            }
        }
    }
}

fn parse_original_tuple(attributes: &[u8]) -> Option<CtTuple> {
    let mut src_v4 = None;
    let mut dst_v4 = None;
    let mut src_v6 = None;
    let mut dst_v6 = None;
    let mut protocol = None;

    for_each_attr(attributes, |attribute_type, payload| {
        if attribute_type != CTA_TUPLE_ORIG {
            return;
        }
        for_each_attr(payload, |tuple_type, tuple_payload| match tuple_type {
            CTA_TUPLE_IP => {
                for_each_attr(tuple_payload, |ip_type, ip_payload| match ip_type {
                    CTA_IP_V4_SRC => {
                        src_v4 = parse_v4(ip_payload);
                    }
                    CTA_IP_V4_DST => {
                        dst_v4 = parse_v4(ip_payload);
                    }
                    CTA_IP_V6_SRC => {
                        src_v6 = parse_v6(ip_payload);
                    }
                    CTA_IP_V6_DST => {
                        dst_v6 = parse_v6(ip_payload);
                    }
                    _ => {}
                });
            }
            CTA_TUPLE_PROTO => {
                for_each_attr(tuple_payload, |proto_type, proto_payload| {
                    if proto_type == CTA_PROTO_NUM && !proto_payload.is_empty() {
                        protocol = Some(proto_payload[0]);
                    }
                });
            }
            _ => {}
        });
    });

    let src = src_v4.map(IpAddr::V4).or_else(|| src_v6.map(IpAddr::V6))?;
    let dst = dst_v4.map(IpAddr::V4).or_else(|| dst_v6.map(IpAddr::V6))?;
    Some(CtTuple {
        src,
        dst,
        protocol: protocol?,
    })
}

fn parse_v4(payload: &[u8]) -> Option<Ipv4Addr> {
    if payload.len() != 4 {
        return None;
    }
    Some(Ipv4Addr::new(
        payload[0], payload[1], payload[2], payload[3],
    ))
}

fn parse_v6(payload: &[u8]) -> Option<Ipv6Addr> {
    if payload.len() != 16 {
        return None;
    }
    let mut octets = [0u8; 16];
    octets.copy_from_slice(payload);
    Some(Ipv6Addr::from(octets))
}

fn for_each_attr(mut data: &[u8], mut callback: impl FnMut(u16, &[u8])) {
    let mut offset = 0usize;
    while offset + NLA_HEADER_LEN <= data.len() {
        let length = read_u16(data, offset).unwrap_or(0) as usize;
        if length < NLA_HEADER_LEN || offset + length > data.len() {
            break;
        }
        let attribute_type = read_u16(data, offset + 2).unwrap_or(0) & NLA_TYPE_MASK;
        callback(
            attribute_type,
            &data[offset + NLA_HEADER_LEN..offset + length],
        );
        offset += align4(length);
    }
    let _ = &mut data;
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn read_u16(buffer: &[u8], offset: usize) -> Option<u16> {
    let slice = buffer.get(offset..offset + 2)?;
    Some(u16::from_ne_bytes([slice[0], slice[1]]))
}

fn read_u32(buffer: &[u8], offset: usize) -> Option<u32> {
    let slice = buffer.get(offset..offset + 4)?;
    Some(u32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_i32(buffer: &[u8], offset: usize) -> Option<i32> {
    let slice = buffer.get(offset..offset + 4)?;
    Some(i32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn write_u16(buffer: &mut [u8], offset: usize, value: u16) {
    buffer[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(attribute_type: u16, payload: &[u8]) -> Vec<u8> {
        let length = NLA_HEADER_LEN + payload.len();
        let mut buffer = vec![0u8; align4(length)];
        buffer[0..2].copy_from_slice(&(length as u16).to_ne_bytes());
        buffer[2..4].copy_from_slice(&attribute_type.to_ne_bytes());
        buffer[4..4 + payload.len()].copy_from_slice(payload);
        buffer
    }

    fn nested(attribute_type: u16, children: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = Vec::new();
        for child in children {
            payload.extend_from_slice(child);
        }
        attr(attribute_type, &payload)
    }

    fn tuple_v4(src: [u8; 4], dst: [u8; 4], protocol: u8) -> Vec<u8> {
        nested(
            CTA_TUPLE_ORIG,
            &[
                nested(
                    CTA_TUPLE_IP,
                    &[attr(CTA_IP_V4_SRC, &src), attr(CTA_IP_V4_DST, &dst)],
                ),
                nested(CTA_TUPLE_PROTO, &[attr(CTA_PROTO_NUM, &[protocol])]),
            ],
        )
    }

    fn tuple_v6(src: [u8; 16], dst: [u8; 16], protocol: u8) -> Vec<u8> {
        nested(
            CTA_TUPLE_ORIG,
            &[
                nested(
                    CTA_TUPLE_IP,
                    &[attr(CTA_IP_V6_SRC, &src), attr(CTA_IP_V6_DST, &dst)],
                ),
                nested(CTA_TUPLE_PROTO, &[attr(CTA_PROTO_NUM, &[protocol])]),
            ],
        )
    }

    fn netlink_message(operation: u8, flags: u16, attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = vec![0u8; NFGENMSG_LEN];
        payload[0] = libc::AF_INET as u8;
        for attribute in attributes {
            payload.extend_from_slice(attribute);
        }
        let length = NLMSG_HEADER_LEN + payload.len();
        let mut message = vec![0u8; length];
        message[0..4].copy_from_slice(&(length as u32).to_ne_bytes());
        message[4..6]
            .copy_from_slice(&((NFNL_SUBSYS_CTNETLINK << 8) | u16::from(operation)).to_ne_bytes());
        message[6..8].copy_from_slice(&flags.to_ne_bytes());
        message[NLMSG_HEADER_LEN..].copy_from_slice(&payload);
        message
    }

    fn done_message(flags: u16) -> Vec<u8> {
        let mut message = vec![0u8; NLMSG_HEADER_LEN];
        message[0..4].copy_from_slice(&(NLMSG_HEADER_LEN as u32).to_ne_bytes());
        message[4..6].copy_from_slice(&NLMSG_DONE.to_ne_bytes());
        message[6..8].copy_from_slice(&flags.to_ne_bytes());
        message
    }

    #[test]
    fn parses_ipv4_dump_entries() {
        let message = netlink_message(
            IPCTNL_MSG_CT_GET,
            0,
            &[tuple_v4([198, 51, 100, 7], [203, 0, 113, 10], 6)],
        );
        let parsed = parse_datagram(&message);
        assert_eq!(
            parsed,
            vec![CtMessage::Entry(CtTuple {
                src: "198.51.100.7".parse().unwrap(),
                dst: "203.0.113.10".parse().unwrap(),
                protocol: 6,
            })]
        );
    }

    #[test]
    fn parses_ipv6_udp_events() {
        let src = "2001:db8:2::9".parse::<Ipv6Addr>().unwrap().octets();
        let dst = "2001:db8:1::10".parse::<Ipv6Addr>().unwrap().octets();
        let new_message =
            netlink_message(IPCTNL_MSG_CT_NEW, NLM_F_CREATE, &[tuple_v6(src, dst, 17)]);
        let update_message = netlink_message(IPCTNL_MSG_CT_NEW, 0, &[tuple_v6(src, dst, 17)]);
        let destroy_message = netlink_message(IPCTNL_MSG_CT_DELETE, 0, &[tuple_v6(src, dst, 17)]);

        let mut combined = new_message.clone();
        combined.extend_from_slice(&update_message);
        combined.extend_from_slice(&destroy_message);

        let parsed = parse_datagram(&combined);
        assert_eq!(parsed.len(), 3);
        assert!(matches!(
            parsed[0],
            CtMessage::Event {
                kind: CtEvent::New,
                ..
            }
        ));
        assert!(matches!(
            parsed[1],
            CtMessage::Event {
                kind: CtEvent::Update,
                ..
            }
        ));
        assert!(matches!(
            parsed[2],
            CtMessage::Event {
                kind: CtEvent::Destroy,
                ..
            }
        ));
    }

    #[test]
    fn parses_done_with_interrupt_flag() {
        let parsed = parse_datagram(&done_message(NLM_F_DUMP_INTR));
        assert_eq!(parsed, vec![CtMessage::Done { interrupted: true }]);
    }

    #[test]
    fn parses_negative_error_codes() {
        let mut message = vec![0u8; NLMSG_HEADER_LEN + 4];
        let length = message.len() as u32;
        message[0..4].copy_from_slice(&length.to_ne_bytes());
        message[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
        message[NLMSG_HEADER_LEN..].copy_from_slice(&(-libc::ENOBUFS).to_ne_bytes());
        assert_eq!(
            parse_datagram(&message),
            vec![CtMessage::Error(-libc::ENOBUFS)]
        );
    }

    #[test]
    fn rejects_truncated_attributes_without_panicking() {
        let mut attributes = tuple_v4([198, 51, 100, 7], [203, 0, 113, 10], 6);
        attributes.truncate(attributes.len() - 2);
        let message = netlink_message(IPCTNL_MSG_CT_GET, 0, &[attributes]);
        let parsed = parse_datagram(&message);
        assert!(parsed.is_empty());
    }

    #[test]
    fn ignores_messages_for_other_subsystems() {
        let mut message = netlink_message(
            IPCTNL_MSG_CT_GET,
            0,
            &[tuple_v4([198, 51, 100, 7], [203, 0, 113, 10], 6)],
        );
        message[4..6].copy_from_slice(&(7u16 << 8).to_ne_bytes());
        assert!(parse_datagram(&message).is_empty());
    }

    #[test]
    fn alignment_rounds_up_to_four_bytes() {
        assert_eq!(align4(4), 4);
        assert_eq!(align4(5), 8);
        assert_eq!(align4(8), 8);
    }
}
