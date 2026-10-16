# printfarm

One window for every 3D printer: state, temperatures, progress, camera, a job
queue and the history of every print. Part of the
[bench suite](https://github.com/v0l/bench-suite).

## Printers

Each printer is reached through a backend behind one `Printer` trait
(`src/printer/mod.rs`), so the queue, history and panel do not care how a
printer is connected.

| backend | how it talks | state |
|---|---|---|
| Marlin over USB | G-code streamed over the serial port, with line numbers, checksums and resends | done |
| Moonraker, OctoPrint, PrusaLink, Bambu LAN | the printer's own host on the network | planned |

A Marlin printer on USB is printed from this computer, so closing printfarm
stops the print. printfarm asks first, and if you go ahead it cancels the print
and turns the heaters off before it exits. A print it lost track of because it
was killed is logged as failed the next time it starts.

Add a printer with "add printer". On Linux the port list offers the
`/dev/serial/by-path` name, which stays the same across replugs as long as the
cable goes in the same USB port. "detect" tries the usual speeds (115200,
250000, 1000000 and others) and keeps the one that answers `M115`.

## Queue and history

Drop G-code files on the window, use "queue g-code", or pass them on the
command line. Each job can be held for one printer or left for any. A printer
card offers the first job it can take, and after a print ends it asks you to
clear the bed before it offers the next one. Nothing starts on its own.

PrusaSlicer's estimate, filament weight, layer count and thumbnail are read
from the file. Progress is the share of the file sent, and the time left blends
the slicer's estimate with the rate so far.

Right click a print in the history to queue it again.

## Camera

Give a printer a camera URL: a JPEG snapshot that is polled once a second, or
an MJPEG stream (`multipart/x-mixed-replace`), such as mjpg-streamer's
`?action=stream`. Click the picture to open it larger.

## Files

| file | Linux | macOS | Windows |
|---|---|---|---|
| `printers.toml` | `~/.config/printfarm` | `~/Library/Application Support/printfarm` | `%APPDATA%\printfarm` |
| `queue.json`, `history.jsonl` | `~/.local/share/printfarm` | `~/Library/Application Support/printfarm` | `%APPDATA%\printfarm` |

`PRINTFARM_HOME` puts all three in one folder instead.

```toml
[[printer]]
id = "ender"
name = "Ender-3 Pro"
kind = "marlin-usb"
port = "/dev/serial/by-path/pci-0000:68:00.0-usb-0:9.4:1.0-port0"
baud = 1000000
camera = "http://camera.local/snapshot.jpg"
```

## Command line

```sh
printfarm [FILES...]     # open the window, queueing any G-code given
printfarm ports          # list serial ports
printfarm probe PORT     # find the speed and firmware of a Marlin printer
printfarm info FILE      # what the slicer wrote into a G-code file
```

## Development

```sh
cargo test
cargo run --example sim /tmp/printfarm-sim   # a simulated Marlin printer on a pty
```

Point a printer at `/tmp/printfarm-sim` to try the panel without hardware. The
simulator heats up slowly, answers temperature reports and checks every line
number and checksum it is sent. The tests in `tests/marlin.rs` run the Marlin
backend against it, including a forced resend, a halted printer, a lost link
and a cancel during a heating wait.

## Release

Move the `Unreleased` entries in `CHANGELOG.md` under the new version, bump
`Cargo.toml`, then tag `v*`. CI builds `printfarm-<target>.tar.gz` (`.zip` on
Windows), a `.dmg` and an `.msi`, with that version's changelog section as the
release notes.
