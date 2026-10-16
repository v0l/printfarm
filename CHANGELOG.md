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

### Added
- Probe the bed from the card, optionally heated first, and see the mesh as a map with its range,
  leveling on or off, fade height and when it was probed. Leveling can be turned on and off and
  the mesh saved to the printer.

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
