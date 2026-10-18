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

## SD cards and USB sticks

When a removable volume is mounted (a USB stick, or an SD card in a reader), a
bar appears under the header with its label, size and free space. "copy queue"
lists the queued jobs, all ticked; the ones you keep are copied to the top
folder of the card as 8.3 names (`BENCHY.GCO`, or `BENCHY~2.GCO` if a different
file already has that name), since that is what the printer's firmware opens.
A file already on the card with the same contents is not copied again. Each
file is written as `.part` and renamed when whole, so stopping the copy leaves
no half file.

The jobs stay in the queue, marked "on card". Put the card in the printer and
start the job from the printer's card: printfarm sends `M23` and `M24` over
serial instead of copying the file, and tracks progress, pause and cancel as
usual. printfarm also lists the printer's own card (`M20`, on connecting and every 30
seconds while idle). A queued job with a file of exactly the same size there (the
name is only used to choose between files of equal size, since the firmware
shortens long names to `BASE~1.GCO`) is started from the card even if printfarm
did not copy it, and a job marked on
card that is no longer there is sent over serial. If the printer cannot find the
file anyway, the job goes back to the top of the
queue as a normal job and is sent over serial. Files on a card you copied are
never deleted by printfarm.

"eject" syncs and unmounts the volume (`udisksctl`, then `umount`) and reports
when it is safe to pull out. Volumes are found on Linux from
`/proc/self/mountinfo` and `/sys`: anything on a USB bus, flagged removable, or
an SD card. Internal disks and `/boot` are never listed. macOS and Windows do
not list volumes yet.

## File manager

"files" on a printer card opens two lists side by side. The left one is the
printer's own SD card, read with `M20`: every file with its size, a "queued"
mark where a queued job matches it, "print" (starts it from the card, and uses
the queued job's progress and history when one matches) and "delete" (`M30`,
after asking). "refresh" reads the card again. Print and delete are off while
the printer is busy, and print is also off while the bed is waiting to be
cleared.

The right one is the mounted USB stick or SD card, with folders you can open,
"queue" for a G-code file and "delete" (after asking). "browse" in the bar
opens the same window. It lists the printer's card only for a connected
printer.

## Bed

"bed" on a card shows the printer's mesh as a map, back at the top and front
at the bottom, with its range, whether leveling is on and the fade height. It
is read with `M420 V` when the printer connects. "probe" homes and runs `G29`,
heating the bed first if you pick a temperature, since a bed moves as it heats.
It is only offered on an idle printer whose bed is marked clear. "save" sends
`M500`, which stores every setting, so it asks first.

## Printers tab

Every printer has an id, a short tag such as `P01` that is filled in when the
printer is added and can be changed in the editor (up to 8 characters, no
spaces, unique). It is shown on the row and card, in the queue's printer
picker, in notices and in the history.

Each printer is one row: state, the job (or what it is waiting for), a progress
bar with the time left, and the control that fits, which is pause and cancel
while printing, start when a job is ready, "bed is clear" after a print, or
reconnect after a fault. The header counts printers by state. "details" opens
the full card for that printer, with temperatures, the bed mesh, camera, files,
console and filament, and "hide" folds it back. "expand all" and "collapse all"
do every printer at once. Rows start collapsed.

## Preview

"preview" on a queued job, or on a printer card while it prints, draws the
part from the G-code itself, so it works for any slicer and for files with no
embedded thumbnail. Each run of extrusion moves is swept into a rounded bead as
wide and as tall as the slicer's layer, with mitred corners, lit by egui_bench's
`viewer3d` on wgpu. Drag to turn it, shift-drag to pan, scroll to zoom, use the
slider to look at the part up to a layer, "top" for a plan view. Travel moves
and retracts are not drawn.

Opened from a printer that is running that file, the window follows the print:
only what has been printed is drawn, the bead being printed now is orange, and
the camera layer tracks it. Drag the slider to stop following and "follow" to
resume; "show rest" draws the unprinted part faintly. The position comes from
the printer's own byte count (`M27`), so a slow first layer moves it slowly.

Big files are simplified so the window stays smooth: straight runs are merged,
then each run of moves that keeps one heading is simplified within a tolerance
that grows until the file has at most about 140,000 beads, but never past
0.2 mm, so infill lines and gyroid waves keep their shape. A 69 MB file with 2.4
million moves reads in under a second and draws about 9 million vertices. The
purge line drawn before the first layer is left out.

Queued jobs show a picture of the part. The slicer's embedded thumbnail is used
when there is one; otherwise the part is drawn from its moves in the background
when it is queued and cached in `thumbs/`. The window needs a GPU with Vulkan,
Metal or DX12; printfarm now runs on wgpu instead of glow.

## Camera

Give a printer a camera URL: a JPEG snapshot that is polled once a second, or
an MJPEG stream (`multipart/x-mixed-replace`), such as mjpg-streamer's
`?action=stream`. Click the picture to open it larger.

## Files

| file | Linux | macOS | Windows |
|---|---|---|---|
| `printers.toml` | `~/.config/printfarm` | `~/Library/Application Support/printfarm` | `%APPDATA%\printfarm` |
| `queue.json`, `history.jsonl` | `~/.local/share/printfarm` | `~/Library/Application Support/printfarm` | `%APPDATA%\printfarm` |
| `thumbs/` | `~/.local/share/printfarm` | `~/Library/Application Support/printfarm` | `%APPDATA%\printfarm` |

`PRINTFARM_HOME` puts them all in one folder instead. `thumbs/` holds the queue's part
pictures drawn from G-code that has no slicer thumbnail; it is safe to delete.

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
printfarm demo [--printers 12]   # a window of simulated printers, nothing touches your config
```

`demo` starts that many simulated Marlin printers (up to 40) in a scratch
folder and opens the window on them. The first twelve are scripted: prints at
different points, one paused, one finished and waiting for the bed to be
cleared, one with a file already on its card, one that hits a thermal runaway
after 30 seconds, one that halts after 6, one offline, one waiting for another
filament, and a history that includes failed and cancelled prints. The folder
is removed when the window closes.

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
