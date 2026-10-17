# Changelog

All notable changes to printfarm are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- A job is copied to the printer's SD card and printed from there, so the printer carries on when
  printfarm closes. On opening, printfarm asks the printer whether its job is still running,
  finished while it was away, or whether it is printing something else.
- Hotend and bed temperatures share one line, and the camera sits beside the card when there is
  room.
- Temperatures are drawn over their sparklines, with the text kept above the line.

### Added
- On Linux, a mounted USB stick or SD card shows a bar with "copy queue" and "eject". Copying lets
  you pick which jobs go, checks the free space, writes 8.3 names the printer can open, skips files
  already on the card and never overwrites a different file. The jobs stay queued, marked on card,
  and the printer starts them from its card with `M23` and `M24` over serial instead of being sent
  the file again. printfarm reads the printer's card with `M20` and uses a file already there when
  its size matches a queued job, whoever put it there. Eject syncs and unmounts, then says when the card can be pulled out.
- A file manager from the printer card's "files" button: the printer's own SD card (list, print,
  delete) beside the mounted USB stick or SD card (browse folders, queue, delete).
- A printer has a loaded filament, set from a chip on its card. Jobs carry the `filament_type` from
  their G-code, a printer only takes jobs that match, and jobs held back for another filament are
  flagged "wrong filament" in the queue. A printer with no filament set takes anything, as does a
  job with none. Jobs queued before this are filled in on opening.
- Probe the bed from the card, optionally heated first, and see the mesh as a map with its range,
  leveling on or off, fade height and when it was probed. Leveling can be turned on and off and
  the mesh saved to the printer.

### Fixed
- Heaters are restored after probing.
- The heating label no longer flickers.

## [0.1.0] - 2026-10-16

First release.

### Added
- A card per printer with its state, temperatures and their recent trace,
  progress, layer, time left and camera.
- Marlin over USB: line numbers, checksums and resends, temperature
  auto-report where the firmware has it, pause, resume and cancel, and speed
  detection.
- A job queue, each job for one printer or any, with PrusaSlicer's estimate,
  filament, layers and thumbnail.
- A history of every print, kept when printfarm is closed, with totals.
- A console per printer.
- Snapshot and MJPEG cameras.
- A simulated Marlin printer for trying it without hardware.
