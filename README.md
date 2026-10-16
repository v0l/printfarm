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
| Marlin over USB | the job is copied to the printer's SD card over the serial port, with line numbers, checksums and resends, then printed from the card | done |
| Moonraker, OctoPrint, PrusaLink, Bambu LAN | the printer's own host on the network | planned |

A printer runs its own job. Starting a job copies it to the printer's SD card
as `PF<job>.GCO` and starts it there, so closing printfarm, a crash or a
pulled USB cable does not stop the print. The only thing printfarm asks before
closing about is a copy that is still going; that job goes back to the top of
the queue.

When printfarm opens, or the printer comes back, it asks the printer what it
is doing (`M27 C`, `M27`) and reports one of three things: the job it sent is
still printing (or paused), that job finished while printfarm was away, or the
printer is printing something printfarm did not send. The file is deleted from
the card once its print ends.

The copy runs at the speed the printer acknowledges lines, so a long print
takes a while to send before it starts.

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

## Bed

"bed" on a card shows the printer's mesh as a map, back at the top and front
at the bottom, with its range, whether leveling is on and the fade height. It
is read with `M420 V` when the printer connects. "probe" homes and runs `G29`,
heating the bed first if you pick a temperature, since a bed moves as it heats.
It is only offered on an idle printer whose bed is marked clear. "save" sends
`M500`, which stores every setting, so it asks first.

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
simulator heats up slowly, checks every line number and checksum it is sent,
and has an SD card that keeps its files and its print while printfarm is
closed. The tests in `tests/marlin.rs` run the Marlin backend against it: a
forced resend during a copy, a print picked up again after printfarm was
closed, one that finished while it was away, one printfarm did not start, a
missing card, a lost link and a halted printer.

## Release

Move the `Unreleased` entries in `CHANGELOG.md` under the new version, bump
`Cargo.toml`, then tag `v*`. CI builds `printfarm-<target>.tar.gz` (`.zip` on
Windows), a `.dmg` and an `.msi`, with that version's changelog section as the
release notes.
