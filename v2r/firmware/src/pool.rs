//! Wi-Fi association and the Stratum client, ported from PicoMiner.

use core::fmt::Write;

use embassy_futures::select::{Either, select};
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_radio::wifi::WifiController;
use heapless::{String, Vec};
use log::{info, warn};
use picominer_core::stratum::{self, Job, LineBuffer, Message, Session};
use picominer_core::target::meets_target;
use static_cell::ConstStaticCell;

use crate::config;
use crate::miner;
use crate::stats::{self, Status};

const ID_SUBSCRIBE: u32 = 1;
const ID_AUTHORIZE: u32 = 2;
const ID_SUGGEST: u32 = 3;
const ID_CONFIGURE: u32 = 4;
const ID_FIRST_SUBMIT: u32 = 5;

const KEEPALIVE: Duration = Duration::from_secs(60);
const JOB_TIMEOUT: Duration = Duration::from_secs(600);
const RSSI_PERIOD: Duration = Duration::from_secs(30);
const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
const DHCP_TIMEOUT: Duration = Duration::from_secs(60);

static RX: ConstStaticCell<[u8; 4096]> = ConstStaticCell::new([0; 4096]);
static TX: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0; 1024]);
static LINES: ConstStaticCell<LineBuffer<4096>> = ConstStaticCell::new(LineBuffer::new());

type Request = String<320>;

/// A share sent and not answered yet: its id, difficulty and whether it met
/// the block target. The pool's answer decides what the screen counts.
#[derive(Clone, Copy)]
struct Pending {
    id: u32,
    difficulty: f64,
    block: bool,
}

async fn send(socket: &mut TcpSocket<'_>, request: &Request) -> Result<(), &'static str> {
    let mut rest = request.as_bytes();
    while !rest.is_empty() {
        let written = socket.write(rest).await.map_err(|_| "write failed")?;
        rest = &rest[written..];
    }
    Ok(())
}

async fn join(controller: &mut WifiController<'static>) {
    stats::update(|stats| stats.status = Status::Joining);
    loop {
        match with_timeout(JOIN_TIMEOUT, controller.connect_async()).await {
            Ok(Ok(_)) => {
                info!("joined {}", config::WIFI_SSID);
                return;
            }
            Ok(Err(error)) => {
                warn!("Wi-Fi join failed: {:?}", error);
                Timer::after_secs(5).await;
            }
            Err(_) => {
                warn!("Wi-Fi join timed out");
                let _ = with_timeout(JOIN_TIMEOUT, controller.disconnect_async()).await;
            }
        }
    }
}

async fn resolve(stack: Stack<'static>, host: &str) -> Result<IpAddress, &'static str> {
    let addresses = stack.dns_query(host, DnsQueryType::A).await.map_err(|_| "DNS failed")?;
    addresses.first().copied().ok_or("no DNS answer")
}

/// One connection to the pool, from subscribe to the first error.
async fn session(
    stack: Stack<'static>,
    controller: &mut WifiController<'static>,
    rx: &mut [u8],
    tx: &mut [u8],
    lines: &mut LineBuffer<4096>,
) -> Result<(), &'static str> {
    let address = resolve(stack, config::POOL_HOST).await?;
    let port = config::port();
    info!("connecting to {}:{}", address, port);

    let mut socket = TcpSocket::new(stack, rx, tx);
    socket.set_timeout(Some(Duration::from_secs(180)));
    socket.set_keep_alive(Some(Duration::from_secs(30)));
    socket.connect((address, port)).await.map_err(|_| "connect failed")?;

    let mut user: String<128> = String::new();
    if config::WORKER.is_empty() {
        write!(user, "{}", config::BTC_ADDRESS)
    } else {
        write!(user, "{}.{}", config::BTC_ADDRESS, config::WORKER)
    }
    .map_err(|_| "user too long")?;

    let mut request = Request::new();
    // Version rolling first, so that it applies from the first job.
    stratum::write_configure(&mut request, ID_CONFIGURE).map_err(|_| "configure")?;
    stratum::write_subscribe(&mut request, ID_SUBSCRIBE, config::AGENT).map_err(|_| "subscribe")?;
    send(&mut socket, &request).await?;

    lines.clear();
    let mut version_mask = 0;
    let mut session: Option<Session> = None;
    let mut job: Option<(u32, Job)> = None;
    let mut difficulty = 1.0;
    let mut next_id = ID_FIRST_SUBMIT;
    let mut pending: Vec<Pending, 16> = Vec::new();
    let mut last_send = Instant::now();
    let mut last_job = Instant::now();
    let mut last_rssi = Instant::from_ticks(0);

    loop {
        match select(socket.read(lines.space()), Timer::after_millis(100)).await {
            Either::First(Ok(0)) => return Err("pool closed"),
            Either::First(Ok(count)) => lines.filled(count),
            Either::First(Err(_)) => return Err("read failed"),
            Either::Second(()) => {}
        }

        while let Some(len) = lines.line_len() {
            match stratum::parse_line(lines.line(len)) {
                Ok(Message::Notify(notify)) => {
                    last_job = Instant::now();
                    if let Some(current) = &session {
                        match Job::build(&notify, current, version_mask) {
                            Ok(built) => {
                                let generation = miner::publish(
                                    &built.header,
                                    &built.versions[..built.version_count],
                                    built.work(),
                                );
                                if let Some(height) = built.height {
                                    stats::update(|stats| stats.height = height);
                                }
                                info!(
                                    "job {} block {:?}, {} versions",
                                    built.job_id.as_str(),
                                    built.height,
                                    built.version_count,
                                );
                                job = Some((generation, built));
                                stats::update(|stats| stats.templates += 1);
                                stats::update(|stats| stats.status = Status::Mining);
                            }
                            Err(error) => warn!("bad job: {:?}", error),
                        }
                    }
                }
                Ok(Message::SetDifficulty(value)) => {
                    info!("pool difficulty {}", value);
                    difficulty = value;
                    miner::set_difficulty(value);
                    stats::update(|stats| stats.pool_difficulty = value);
                }
                Ok(Message::SetVersionMask(mask)) => {
                    info!("version mask {:08x}", mask);
                    version_mask = mask;
                }
                Ok(Message::Reply { id: ID_CONFIGURE, result, error }) => {
                    version_mask = if error.is_some() { 0 } else { stratum::parse_configure(&result) };
                    info!("version rolling mask {:08x}", version_mask);
                }
                Ok(Message::Reply { id: ID_SUBSCRIBE, result, error }) => {
                    if let Some(reason) = error {
                        warn!("subscribe refused: {}", reason);
                        return Err("subscribe refused");
                    }
                    let subscribed = stratum::parse_subscribe(&result).map_err(|_| "subscribe reply")?;
                    info!(
                        "subscribed, extranonce1 {} extranonce2 {} bytes",
                        subscribed.extranonce1.as_str(),
                        subscribed.extranonce2_size,
                    );
                    session = Some(subscribed);
                    request.clear();
                    stratum::write_authorize(&mut request, ID_AUTHORIZE, &user, "x").map_err(|_| "authorize")?;
                    stratum::write_suggest_difficulty(&mut request, ID_SUGGEST, config::SUGGESTED_DIFFICULTY)
                        .map_err(|_| "suggest")?;
                    send(&mut socket, &request).await?;
                    last_send = Instant::now();
                }
                Ok(Message::Reply { id: ID_AUTHORIZE, result, error }) => {
                    if error.is_some() || result.as_bool() == Some(false) {
                        warn!("authorize refused: {:?}", error);
                        return Err("authorize refused");
                    }
                    info!("authorized as {}", user.as_str());
                }
                Ok(Message::Reply { id, result, error }) if id >= ID_FIRST_SUBMIT => {
                    let accepted = error.is_none() && result.as_bool() != Some(false);
                    let share = pending.iter().position(|share| share.id == id).map(|at| pending.swap_remove(at));
                    if let (true, Some(share)) = (accepted, share) {
                        stats::update(|stats| {
                            stats.best = stats.best.max(share.difficulty);
                            if share.difficulty >= 1.0 {
                                stats.shares32 += 1;
                            }
                            if share.block {
                                stats.blocks += 1;
                            }
                        });
                    }
                    if accepted {
                        info!("share {} accepted", id);
                    } else {
                        warn!("share {} rejected: {:?}", id, error);
                    }
                    stats::update(|stats| {
                        if accepted {
                            stats.accepted += 1;
                        } else {
                            stats.rejected += 1;
                        }
                    });
                }
                Ok(_) => {}
                Err(error) => warn!("bad line: {:?}", error),
            }
            lines.consume(len);
        }

        while let Some(found) = miner::take_found() {
            let Some((generation, current)) = &job else { continue };
            if found.generation != *generation || found.difficulty < difficulty {
                continue;
            }
            let block = meets_target(&found.hash, &current.target);
            if block {
                warn!("BLOCK FOUND: nonce {:08x}", found.nonce);
            }
            if pending.is_full() {
                pending.remove(0);
            }
            let _ = pending.push(Pending { id: next_id, difficulty: found.difficulty, block });
            info!("submitting diff {:.6}, version {}, as {}", found.difficulty, found.version, next_id);
            request.clear();
            stratum::write_submit(&mut request, next_id, &user, current, found.nonce, found.version)
                .map_err(|_| "submit")?;
            send(&mut socket, &request).await?;
            next_id += 1;
            last_send = Instant::now();
        }

        let now = Instant::now();
        if now - last_rssi >= RSSI_PERIOD {
            last_rssi = now;
            if let Ok(rssi) = controller.rssi() {
                stats::update(|stats| stats.rssi = rssi);
            }
        }
        if now - last_send >= KEEPALIVE {
            request.clear();
            stratum::write_suggest_difficulty(&mut request, ID_SUGGEST, config::SUGGESTED_DIFFICULTY)
                .map_err(|_| "suggest")?;
            send(&mut socket, &request).await?;
            last_send = now;
        }
        if now - last_job >= JOB_TIMEOUT {
            return Err("no job for 10 minutes");
        }
    }
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, mut controller: WifiController<'static>) -> ! {
    let (rx, tx, lines) = (RX.take(), TX.take(), LINES.take());
    loop {
        if !controller.is_connected() {
            join(&mut controller).await;
        }
        stats::update(|stats| stats.status = Status::Dhcp);
        if with_timeout(DHCP_TIMEOUT, stack.wait_config_up()).await.is_err() {
            warn!("no DHCP lease");
            let _ = with_timeout(JOIN_TIMEOUT, controller.disconnect_async()).await;
            continue;
        }
        if let Some(config) = stack.config_v4() {
            info!("address {}", config.address);
            let octets = config.address.address().octets();
            stats::update(|stats| stats.ip = octets);
        }
        stats::update(|stats| stats.status = Status::Connecting);

        let started = Instant::now();
        if let Err(reason) = session(stack, &mut controller, rx, tx, lines).await {
            warn!(
                "pool session ended after {} s, link {}: {}",
                started.elapsed().as_secs(),
                controller.is_connected(),
                reason,
            );
            stats::update(|stats| stats.drops += 1);
        }
        miner::stop();
        stats::update(|stats| stats.status = Status::PoolError);
        Timer::after_secs(3).await;
    }
}
