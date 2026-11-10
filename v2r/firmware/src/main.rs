//! NerdMiner v2R: a solo Bitcoin lottery miner for the ESP32-S3.
//!
//! Core 0 drives the SHA accelerator in a bare loop, without a scheduler or a
//! single interrupt. Core 1 runs esp-rtos: Wi-Fi, the network stack and the
//! pool in an embassy executor, and a software miner below them that takes
//! whatever time they leave.

#![no_std]
#![no_main]
// Inline assembly on Xtensa, for the funnel shift in `sw`.
#![feature(asm_experimental_arch)]

extern crate alloc;

mod buttons;
mod clock;
mod config;
mod display;
// Every size the C++ screens use; the screens not ported yet need the rest.
#[allow(dead_code)]
#[rustfmt::skip]
mod fonts {
    include!("../assets/fonts.rs");
}
mod hw;
mod lcd;
mod miner;
mod netdata;
mod pool;
mod stats;
mod sw;
mod temperature;
mod text;

use core::ffi::c_void;
use core::ptr::addr_of_mut;

use esp_rtos::embassy::Executor;
use embassy_net::{Runner, StackResources};
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{AnyPin, Input, InputConfig, Pull};
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
static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();

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

/// The board's peripherals that core 1 drives.
struct Board {
    wifi: esp_hal::peripherals::WIFI<'static>,
    lcd: lcd::Pins,
    /// The BOOT button, on the USB side, and the other one.
    left: AnyPin<'static>,
    right: AnyPin<'static>,
}

/// Everything but the accelerator loop, on core 1 under esp-rtos.
fn core1(board: Board) {
    let wifi = board.wifi;
    esp_rtos::CurrentThreadHandle::get().set_priority(EXECUTOR_PRIORITY);

    let mut controller = WifiController::new(wifi, Default::default()).expect("Wi-Fi starts");
    controller.set_config(&station_config()).expect("Wi-Fi configuration");
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let tls_seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
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
        spawner.spawn(clock::run(stack).expect("clock task"));
        spawner.spawn(netdata::run(stack, tls_seed).expect("network data task"));
        let button = InputConfig::default().with_pull(Pull::Up);
        let left = buttons::Button::new(Input::new(board.left, button));
        let right = buttons::Button::new(Input::new(board.right, button));
        spawner.spawn(display::watch(0, left).expect("left button task"));
        spawner.spawn(display::watch(1, right).expect("right button task"));
        spawner.spawn(display::run(lcd::Lcd::new(board.lcd)).expect("display task"));
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
    let board = Board {
        wifi: peripherals.WIFI,
        lcd: lcd::Pins {
            power: peripherals.GPIO15.into(),
            backlight: peripherals.GPIO38.into(),
            reset: peripherals.GPIO5.into(),
            cs: peripherals.GPIO6.into(),
            dc: peripherals.GPIO7.into(),
            wr: peripherals.GPIO8.into(),
            rd: peripherals.GPIO9.into(),
            data: [
                peripherals.GPIO39.into(),
                peripherals.GPIO40.into(),
                peripherals.GPIO41.into(),
                peripherals.GPIO42.into(),
                peripherals.GPIO45.into(),
                peripherals.GPIO46.into(),
                peripherals.GPIO47.into(),
                peripherals.GPIO48.into(),
            ],
        },
        left: peripherals.GPIO0.into(),
        right: peripherals.GPIO14.into(),
    };
    let stack = unsafe { &mut *addr_of_mut!(CORE1_STACK) };
    esp_rtos::start_on_second_core_only(
        peripherals.CPU_CTRL,
        peripherals.FROM_CPU_INTR1,
        timg0.timer0,
        stack,
        move || core1(board),
    );

    // From here core 0 must not call any esp-rtos API.
    let _sha = sha;
    miner::mine_hw()
}
