//! Wall-clock time from NTP, as the C++ firmware's NTPClient gets it: from
//! europe.pool.ntp.org, again every hour, shifted by the configured time zone.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use log::{info, warn};

use crate::config;

const SERVER: &str = "europe.pool.ntp.org";
const PERIOD: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(60);
/// Seconds from 1900, NTP's era, to 1970.
const UNIX_FROM_NTP: u32 = 2_208_988_800;

/// Unix time at boot, 0 until NTP answers.
static BOOT_EPOCH: AtomicU32 = AtomicU32::new(0);

/// Local time in seconds since 1970, counted from boot while NTP is unknown,
/// as NTPClient does.
pub fn local_now() -> u32 {
    let since_boot = Instant::now().as_secs() as u32;
    let zone = config::timezone() * 3600;
    BOOT_EPOCH.load(Ordering::Relaxed).wrapping_add(since_boot).wrapping_add_signed(zone)
}

/// `HH:MM`, as `getTime` prints it.
pub fn hours_minutes() -> (u32, u32) {
    let seconds = local_now() % 86_400;
    (seconds / 3600, seconds / 60 % 60)
}

async fn query(stack: Stack<'static>) -> Result<u32, &'static str> {
    let address = *stack
        .dns_query(SERVER, DnsQueryType::A)
        .await
        .map_err(|_| "DNS failed")?
        .first()
        .ok_or("no DNS answer")?;
    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let (mut rx, mut tx) = ([0u8; 64], [0u8; 64]);
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx, &mut tx_meta, &mut tx);
    socket.bind(0).map_err(|_| "bind failed")?;
    let mut request = [0u8; 48];
    // Leap indicator unknown, version 4, client mode.
    request[0] = 0xe3;
    socket.send_to(&request, (address, 123)).await.map_err(|_| "send failed")?;
    let mut answer = [0u8; 48];
    let (length, _) = with_timeout(Duration::from_secs(5), socket.recv_from(&mut answer))
        .await
        .map_err(|_| "timed out")?
        .map_err(|_| "receive failed")?;
    if length < 48 {
        return Err("short answer");
    }
    let seconds = u32::from_be_bytes([answer[40], answer[41], answer[42], answer[43]]);
    Ok(seconds.wrapping_sub(UNIX_FROM_NTP))
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>) -> ! {
    loop {
        stack.wait_config_up().await;
        match query(stack).await {
            Ok(unix) => {
                let boot = unix.wrapping_sub(Instant::now().as_secs() as u32);
                BOOT_EPOCH.store(boot, Ordering::Relaxed);
                info!("NTP time {}", unix);
                Timer::after(PERIOD).await;
            }
            Err(reason) => {
                warn!("NTP failed: {}", reason);
                Timer::after(RETRY).await;
            }
        }
    }
}
