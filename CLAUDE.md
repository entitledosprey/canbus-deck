# canbus-deck

Rust CAN capture and reverse engineering tool for Linux. The binary is `candeck`. `README.md` covers install and use. If `CLAUDE.local.md` exists, read it for local hardware and test setup.

## Layout

| Path | Purpose |
|---|---|
| `crates/candeck/src/can.rs` | Raw SocketCAN: receive with kernel timestamps and drop counts, `VcanTx` |
| `crates/candeck/src/detect.rs` | Listen-only bitrate sweep |
| `crates/candeck/src/daemon.rs` | Capture daemon, log writer, marker commands, client socket |
| `crates/candeck/src/wire.rs` | 24-byte records streamed to clients |
| `crates/candeck/src/tui.rs` | Live view, keys, typed test names |
| `crates/candeck/src/compare.rs` | Marker parsing, pulse scan, level and byte comparison |
| `crates/candeck/src/senders.rs` | Clock-drift sender grouping, IDs that went quiet |
| `crates/candeck/src/sim.rs` | Simulated traffic for vcan |
| `crates/can-bench/` | Throughput test, vcan only |
| `scripts/` | Desktop launcher, desktop entry and installer |
| `.github/workflows/release.yml` | Builds static binaries and publishes a release on `v*` tags |

## Safety rules

These protect a running vehicle. Don't weaken them without an explicit request.

- `CanSock` has no send method. `VcanTx` is the only transmit path, and it refuses anything but `vcan*`.
- `detect` sets listen-only while the link is down, reads it back from the kernel, and aborts before link up if it isn't confirmed.
- `daemon` refuses a real interface that isn't listen-only.
- Any future transmit feature must be armed explicitly per session, and stay out of the web UI at first.

## Working on it

SocketCAN is Linux-only, so build and test on a Linux machine.

```
cargo test --release -p candeck
cargo clippy --release -p candeck -- -D warnings
cargo build --release -p candeck
scripts/install.sh
```

Before calling something done, run tests and clippy, then try the real path with `vcan0` (`candeck daemon`, `sim`, `sniff`). `vcan0` doesn't survive a reboot; the README has the setup commands.

Scripted key presses work through `script`, as in `(sleep 3; printf 1; sleep 1; printf 1; sleep 1; printf q) | CANDECK_IFACE=vcan0 CANDECK_BIN=$PWD/target/release/candeck script -qec scripts/candeck-start /dev/null`.

## Releasing

Update `version` in the root `Cargo.toml`, commit, then push a `v*` tag. The workflow tests, builds both architectures and publishes the release. `candeck-start` stays a script, so release archives must keep it beside the binary.

## Conventions

- The standard test is one block per action: press to start, repeat the action, press to stop. Don't make the user mark each repeat.
- Keyboards on small devices can be awkward, so prefer single keys and the desktop icon over typed commands.
- Keep log files plain candump. Extra data goes in sidecar files, as `.markers` does.
- Report uncertainty in output, and refuse to guess when the data can't support an answer, as the short capture check does.
- Thresholds that came from real data say so in a comment.
- `README.md` only covers install, use and troubleshooting. Don't add status, known issues or project history.
- Commit messages are short and plain, with no trailers or attribution lines.
- Keep personal details, hardware addresses and vehicle data out of tracked files. They belong in `CLAUDE.local.md`, which is ignored.

## Planned

- Web UI (`axum`), read-only by default, requiring a login or reached through an SSH tunnel
- DBC import and export, live signal definitions, and signal charts
- GPS tagging of captures and SQLite storage
- Transmit, replay, and UDS over ISO-TP, under the safety rules above
