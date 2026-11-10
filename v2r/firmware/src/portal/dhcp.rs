//! The access point's DHCP server, as the IDF soft AP runs it under
//! WiFiManager: addresses from 192.168.4.2, with the portal as the router and
//! the DNS server.

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Ipv4Address, Stack};
use heapless::Vec;
use log::{debug, warn};

use super::AP_ADDRESS;

const SERVER_PORT: u16 = 67;
const CLIENT_PORT: u16 = 68;
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const LEASE_SECONDS: u32 = 7200;
const FIRST_HOST: u8 = 2;
const MAX_LEASES: usize = 8;

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
const RELEASE: u8 = 7;

struct Request {
    kind: u8,
    xid: [u8; 4],
    flags: [u8; 2],
    ciaddr: [u8; 4],
    chaddr: [u8; 16],
    requested: Option<[u8; 4]>,
}

fn parse(packet: &[u8]) -> Option<Request> {
    // op 1 (request), Ethernet hardware addresses.
    if packet.len() < 240 || packet[0] != 1 || packet[1] != 1 || packet[2] != 6 || packet[236..240] != COOKIE {
        return None;
    }
    let mut request = Request {
        kind: 0,
        xid: packet[4..8].try_into().ok()?,
        flags: packet[10..12].try_into().ok()?,
        ciaddr: packet[12..16].try_into().ok()?,
        chaddr: packet[28..44].try_into().ok()?,
        requested: None,
    };
    let mut options = &packet[240..];
    while let [code, rest @ ..] = options {
        match code {
            0 => options = rest,
            255 => break,
            _ => {
                let [length, rest @ ..] = rest else { break };
                let length = usize::from(*length);
                if rest.len() < length {
                    break;
                }
                let value = &rest[..length];
                match (code, value) {
                    (53, [kind]) => request.kind = *kind,
                    (50, [a, b, c, d]) => request.requested = Some([*a, *b, *c, *d]),
                    _ => {}
                }
                options = &rest[length..];
            }
        }
    }
    Some(request)
}

fn reply(out: &mut [u8; 300], request: &Request, kind: u8, address: [u8; 4]) -> usize {
    out.fill(0);
    out[0] = 2;
    out[1] = 1;
    out[2] = 6;
    out[4..8].copy_from_slice(&request.xid);
    out[10..12].copy_from_slice(&request.flags);
    if kind != NAK {
        out[16..20].copy_from_slice(&address);
    }
    out[20..24].copy_from_slice(&AP_ADDRESS.octets());
    out[28..44].copy_from_slice(&request.chaddr);
    out[236..240].copy_from_slice(&COOKIE);
    let server = AP_ADDRESS.octets();
    let mut at = 240;
    let mut option = |code: u8, value: &[u8]| {
        out[at] = code;
        out[at + 1] = value.len() as u8;
        out[at + 2..at + 2 + value.len()].copy_from_slice(value);
        at += 2 + value.len();
    };
    option(53, &[kind]);
    option(54, &server);
    if kind != NAK {
        option(51, &LEASE_SECONDS.to_be_bytes());
        option(1, &[255, 255, 255, 0]);
        option(3, &server);
        option(6, &server);
    }
    out[at] = 255;
    at + 1
}

/// The address of a client, given out on its first discover.
fn lease(leases: &mut Vec<[u8; 6], MAX_LEASES>, chaddr: &[u8; 16]) -> [u8; 4] {
    let mac: [u8; 6] = chaddr[..6].try_into().unwrap_or_default();
    let index = match leases.iter().position(|known| *known == mac) {
        Some(index) => index,
        None => {
            if leases.is_full() {
                leases.remove(0);
            }
            let _ = leases.push(mac);
            leases.len() - 1
        }
    };
    let [a, b, c, _] = AP_ADDRESS.octets();
    [a, b, c, FIRST_HOST + index as u8]
}

#[embassy_executor::task]
pub async fn serve(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(SERVER_PORT).expect("DHCP port");
    let mut leases: Vec<[u8; 6], MAX_LEASES> = Vec::new();
    let mut packet = [0u8; 576];
    let mut out = [0u8; 300];
    loop {
        let Ok((length, _)) = socket.recv_from(&mut packet).await else {
            continue;
        };
        let Some(request) = parse(&packet[..length]) else {
            continue;
        };
        let address = lease(&mut leases, &request.chaddr);
        let kind = match request.kind {
            DISCOVER => OFFER,
            REQUEST => {
                let asked = request.requested.unwrap_or(request.ciaddr);
                if asked == address { ACK } else { NAK }
            }
            RELEASE => continue,
            _ => continue,
        };
        debug!("DHCP {} -> {:?} {:?}", request.kind, kind, address);
        let length = reply(&mut out, &request, kind, address);
        let broadcast = IpAddress::Ipv4(Ipv4Address::BROADCAST);
        if socket.send_to(&out[..length], (broadcast, CLIENT_PORT)).await.is_err() {
            warn!("DHCP reply not sent");
        }
    }
}
