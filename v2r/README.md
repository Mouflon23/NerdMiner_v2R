# NerdMiner v2R

The Rust rewrite of NerdMiner v2 for the ESP32-S3, starting with the board this
fork is tested on: the LilyGo T-Display S3 (`NerdminerV2` in `platformio.ini`).
The C++ firmware stays in the repository as the reference until the Rust one
matches it.

## Building

Install the Xtensa Rust toolchain once:

```powershell
cargo install espup espflash --locked
espup install --targets esp32s3
```

Then, in each new PowerShell window, put the Xtensa linker on `PATH` and build:

```powershell
. ./v2r/env.ps1
cd v2r/bench
cargo build --release
espflash flash --chip esp32s3 --monitor target/xtensa-esp32s3-none-elf/release/v2r-bench
```

## `bench`: the SHA-256 kernels

`bench` checks each mining kernel against the software reference from
[PicoMiner](https://github.com/Mouflon23/PicoMiner)'s core, on 2000 nonces, and
then times it. Measured on 9 Nov. 2026 on the T-Display S3 at 240 MHz, with
neither Wi-Fi nor screen running:

| Kernel | What it does | kH/s |
| --- | --- | --- |
| hw upstream | NerdMiner v1.8.3's loop: every message word of both blocks, every nonce | 231 |
| hw minimal | never rewrites message words 9..=14, zero in both blocks | 280 |
| hw pipe2 | fills the next nonce's first block while the accelerator hashes the second | 311 |
| hw pipe12 | pipe2, plus the second block's padding written during the first | 324 |
| hw fixed | pipe12 waiting a calibrated number of cycles instead of polling `BUSY` | 343 |
| sw rust ×1 | PicoMiner's scan, compiled for the Xtensa LX7 | 28 |
| sw rust ×4 | the same with four header versions per nonce (BIP 310) | 34 |
| **dual core** | **hw pipe12 on core 0, sw ×4 on core 1** | **358** |

For comparison, the stock C++ firmware shows about 253 kH/s on the same board, with Wi-Fi and the screen running.

One nonce of `hw minimal` takes about 850 cycles, and only 340 of them are
the accelerator hashing. The rest go to peripheral bus accesses: about
12 cycles per register write and 20 per read. Every gain above comes from hiding those
accesses behind the hashing. The accelerator takes in its message words as soon as
a block starts, so the next words can be written while it works.

`hw fixed` needs the delays measured for this clock. The firmware will wait
most of that delay and then poll `BUSY` once, which keeps it right if the clock
ever changes.
