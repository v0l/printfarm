# Changelog

All notable changes to printfarm are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
