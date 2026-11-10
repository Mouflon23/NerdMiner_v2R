//! Clicks, double clicks and long presses, with the OneButton timings the C++
//! firmware uses: a click ends 400 ms after release if no second press comes,
//! and a press longer than 800 ms is a long press, reported as soon as it is
//! one, as `attachLongPressStart` does.

use embassy_time::{Duration, Instant, Timer};
use esp_hal::gpio::Input;

const DEBOUNCE: Duration = Duration::from_millis(50);
const CLICK: Duration = Duration::from_millis(400);
const LONG_PRESS: Duration = Duration::from_millis(800);
const POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Click,
    DoubleClick,
    /// Three clicks or more.
    MultiClick,
    LongPress,
}

pub struct Button {
    input: Input<'static>,
    /// Still down after a long press, which must not start another gesture.
    held: bool,
}

impl Button {
    pub fn new(input: Input<'static>) -> Self {
        Self { input, held: false }
    }

    fn pressed(&self) -> bool {
        self.input.is_low()
    }

    /// Waits for the next gesture.
    pub async fn next(&mut self) -> Event {
        if self.held {
            self.input.wait_for_high().await;
            Timer::after(DEBOUNCE).await;
            self.held = false;
        }
        loop {
            self.input.wait_for_low().await;
            Timer::after(DEBOUNCE).await;
            if !self.pressed() {
                continue;
            }
            let mut clicks = 0u32;
            loop {
                // Held down: a long press, or a click once released.
                let down = Instant::now();
                while self.pressed() {
                    if clicks == 0 && down.elapsed() >= LONG_PRESS {
                        self.held = true;
                        return Event::LongPress;
                    }
                    Timer::after(POLL).await;
                }
                clicks += 1;
                Timer::after(DEBOUNCE).await;
                // Released: another press soon makes it a multiple click.
                let up = Instant::now();
                while !self.pressed() && up.elapsed() < CLICK {
                    Timer::after(POLL).await;
                }
                if !self.pressed() {
                    return match clicks {
                        1 => Event::Click,
                        2 => Event::DoubleClick,
                        _ => Event::MultiClick,
                    };
                }
            }
        }
    }
}
