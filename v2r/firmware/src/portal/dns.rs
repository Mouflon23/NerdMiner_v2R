//! WiFiManager's `DNSServer`: every name resolves to the portal, which is
//! what makes phones and laptops open it by themselves.

use embassy_net::Stack;
use embassy_net::udp::{PacketMetadata, UdpSocket};

use super::AP_ADDRESS;

const PORT: u16 = 53;
const TYPE_A: u16 = 1;
const TTL_SECONDS: u32 = 60;

/// The answer to `query` in `out`, or `None` to ignore it.
fn answer(query: &[u8], out: &mut [u8; 512]) -> Option<usize> {
    // One question, and a query rather than an answer.
    if query.len() < 12 || query[2] & 0x80 != 0 || query[4..6] != [0, 1] {
        return None;
    }
    // The question: labels up to the root, then the type and class.
    let mut end = 12;
    loop {
        let length = usize::from(*query.get(end)?);
        end += 1 + length;
        if length == 0 {
            break;
        }
        if length > 63 {
            return None;
        }
    }
    let kind = u16::from_be_bytes([*query.get(end)?, *query.get(end + 1)?]);
    end += 4;
    if end > query.len() || end + 16 > out.len() {
        return None;
    }
    out[..end].copy_from_slice(&query[..end]);
    // An answer, recursion desired copied, recursion available, no error.
    out[2] = 0x80 | (query[2] & 0x01);
    out[3] = 0x80;
    out[6..12].fill(0);
    if kind != TYPE_A {
        return Some(end);
    }
    out[7] = 1;
    let record = &mut out[end..end + 16];
    record[0..2].copy_from_slice(&[0xc0, 0x0c]);
    record[2..4].copy_from_slice(&TYPE_A.to_be_bytes());
    record[4..6].copy_from_slice(&[0, 1]);
    record[6..10].copy_from_slice(&TTL_SECONDS.to_be_bytes());
    record[10..12].copy_from_slice(&[0, 4]);
    record[12..16].copy_from_slice(&AP_ADDRESS.octets());
    Some(end + 16)
}

#[embassy_executor::task]
pub async fn serve(stack: Stack<'static>) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(PORT).expect("DNS port");
    let mut query = [0u8; 512];
    let mut out = [0u8; 512];
    loop {
        let Ok((length, from)) = socket.recv_from(&mut query).await else {
            continue;
        };
        if let Some(length) = answer(&query[..length], &mut out) {
            let _ = socket.send_to(&out[..length], from.endpoint).await;
        }
    }
}
