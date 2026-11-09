//! NerdMiner v2R: a solo Bitcoin lottery miner for the ESP32-S3.
//!
//! Core 0 drives the SHA accelerator in a bare loop, without a scheduler or a
//! single interrupt. Core 1 runs esp-rtos: Wi-Fi, the network stack and the
//! pool in an embassy executor, and a software miner below them that takes
//! whatever time they leave.

#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod hw;
mod miner;
mod pool;
mod stats;

use core::ffi::c_void;
use core::ptr::addr_of_mut;

use esp_rtos::embassy::Executor;
use embassy_net::{Runner, StackResources};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::sha::Sha;
use esp_hal::system::Stack;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{AuthenticationMethodConfig, Config, Interface, WifiController};
use log::info;
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

static mut CORE1_STACK: Stack<32768> = Stack::new();
static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();

/// Above the software miner, so that the pool never waits for it.
const EXECUTOR_PRIORITY: usize = 2;
const SW_MINER_PRIORITY: u32 = 1;
const SW_MINER_STACK: usize = 8192;

#[embassy_executor::task]
async fn net(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

extern "C" fn sw_miner(_: *mut c_void) {
    miner::mine_sw()
}

fn station_config() -> Config {
    let authentication = if config::WIFI_PASSWORD.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(config::WIFI_PASSWORD.try_into().expect("Wi-Fi password too long"))
    };
    Config::Station(
        StationConfig::default()
            .with_ssid(config::WIFI_SSID.try_into().expect("SSID too long"))
            .with_authentication(authentication),
    )
}

/// Everything but the accelerator loop, on core 1 under esp-rtos.
fn core1(wifi: esp_hal::peripherals::WIFI<'static>) {
    esp_rtos::CurrentThreadHandle::get().set_priority(EXECUTOR_PRIORITY);

    let mut controller = WifiController::new(wifi, Default::default()).expect("Wi-Fi starts");
    controller.set_config(&station_config()).expect("Wi-Fi configuration");
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let (stack, runner) = embassy_net::new(
        Interface::station(),
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    unsafe {
        esp_radio_rtos_driver::task_create(
            "sw-miner",
            sw_miner,
            core::ptr::null_mut(),
            SW_MINER_PRIORITY,
            Some(1),
            SW_MINER_STACK,
        );
    }

    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(net(runner).expect("net task"));
        spawner.spawn(pool::run(stack, controller).expect("pool task"));
        spawner.spawn(stats::report().expect("stats task"));
    })
}

#[esp_hal::main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);
    info!("{} for {}, pool {}:{}", config::AGENT, config::BTC_ADDRESS, config::POOL_HOST, config::POOL_PORT);

    // Keeps the accelerator clocked for good.
    let sha = Sha::new(peripherals.SHA);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let wifi = peripherals.WIFI;
    let stack = unsafe { &mut *addr_of_mut!(CORE1_STACK) };
    esp_rtos::start_on_second_core_only(
        peripherals.CPU_CTRL,
        peripherals.FROM_CPU_INTR1,
        timg0.timer0,
        stack,
        move || core1(wifi),
    );

    // From here core 0 must not call any esp-rtos API.
    let _sha = sha;
    miner::mine_hw()
}
