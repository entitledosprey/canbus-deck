# candeck

A CAN bus capture and reverse engineering tool for Linux. It finds a bus's bitrate, records traffic, shows it live in the terminal, and helps you work out which bits and bytes respond to an action, such as pressing a pedal.

candeck only listens on real CAN interfaces. It sets the adapter to listen-only mode, confirms that mode with the kernel, and has no way to transmit except on a virtual `vcan` interface.

## Requirements

- Linux on 64-bit ARM or x86, with SocketCAN and `iproute2` for the `ip` command
- A SocketCAN adapter, such as a PEAK PCAN-USB, which uses the `peak_usb` kernel driver

## Install

Run this, which needs `curl`, `sha256sum` and `sudo`:

```
curl -fsSL https://raw.githubusercontent.com/entitledosprey/canbus-deck/main/scripts/install.sh | sh
```

This downloads the latest release, verifies its checksum, installs `candeck` and `candeck-start` to `/usr/local/bin`, and adds a **CAN Sniffer** icon to your desktop and app menu. Run it again to update. There's nothing to compile, because the binaries have no dependencies.

If you'd rather install by hand, download the archive for your system from the [releases page](https://github.com/entitledosprey/canbus-deck/releases), extract it, and run `./install.sh` from inside the folder.

### Build from source

You only need this to change the code. Install Rust from [rustup.rs](https://rustup.rs) and a C compiler, then run:

```
git clone https://github.com/entitledosprey/canbus-deck.git
cd canbus-deck
cargo build --release -p candeck
scripts/install.sh
```

After rebuilding, run `scripts/install.sh` again to update the installed copy.

### Running the desktop icon without a password

The desktop icon finds the bitrate with `sudo -n`, so it needs to run that one command without a password. To allow only that command, run `sudo visudo -f /etc/sudoers.d/candeck-detect` and add this line, using your own username:

```
yourname ALL=(root) NOPASSWD: /usr/local/bin/candeck detect can0
```

## Quick start

1. Connect the adapter to the bus. For a car, that's usually the OBD port with the ignition on.
2. Double-click **CAN Sniffer**. If your file manager asks what to do, choose **Execute**.
3. Wait up to about 20 seconds while it finds the bitrate. The live view opens when it's done.
4. Mark your tests using the keys below.
5. Press `q`. Capture stops and the results appear.

To do the same from a terminal, run each of these in its own window:

```
sudo candeck detect can0
candeck daemon can0
candeck sniff
```

Stop the daemon with `Ctrl+C` when you're done, then run `candeck compare` to see the results.

## Testing an action

Mark one block per action:

1. Press the action's key to start.
2. Do the action as many times as you like, and count how many.
3. Press the same key to stop.

| Key | Action |
|---|---|
| `1` | Brake |
| `2` | Throttle |
| `3` | Steering |
| `4` | Clutch |
| `5` | Lights |
| `6` | Anything else: type what you're testing and press `Enter`, then press `6` to stop |

Leave a few seconds of doing nothing before and after each block, so there's a resting state to compare against. You can press `Esc` to cancel while typing a name.

Other keys in the live view:

| Key | Action |
|---|---|
| `s` | Switch between sorting by ID and by rate |
| `Space` | Pause the display. Capture keeps running |
| `q` | Quit |

Highlighted bytes changed within the last moment. The top line shows the frame rate, the number of IDs, bus errors, and any frames lost.

## Reading the results

`candeck compare` reads the newest capture. Pass a path to read a different one, or use `--label` to show one action:

```
candeck compare
candeck compare ~/.local/share/candeck/captures/1700000000-can0.log --label brake
```

Each action has up to three sections, strongest evidence first.

**Bits that pulsed only during this action** are your best candidates. Compare the pulse count with the number of times you did the action. Five brake presses should give five pulses. Bits listed together changed at the same moments. A very high pulse count usually means part of a changing value, not a switch.

```
== brake: 1 block
   Bits that pulsed only during this action. Check the count against what you did:
      5 pulses   1A0 2.5, 2B4 6.4
```

Bit positions are written as `byte.bit`, so `1A0 2.5` is ID `0x1A0`, byte 2, bit 5, with bit 0 as the least significant bit.

**Bits at a different level during the action** are weaker evidence. They may be real, or they may be something that changed around the same time.

**Bytes moving outside their resting range** are useful for gradual values such as pedal position or steering angle, which don't produce clean pulses.

### Likely senders

Results end with groups of IDs that appear to come from the same module, based on tiny differences between each module's clock:

```
== Likely senders (IDs that share a clock)
   A      -36 ppm   1A0 1A1 1A2
   B      -10 ppm   2B4 2B5 3C0 3C1
```

A group is evidence of a shared sender, but it doesn't name the module. Captures need at least 40 seconds for this, and about 90 seconds to group slow IDs. To see only this section, run `candeck senders`.

## Commands

| Command | What it does |
|---|---|
| `candeck detect <iface>` | Finds the bitrate and leaves the link up in listen-only mode. Needs root. Add `--window <seconds>` to change the listening time per rate |
| `candeck daemon <iface>` | Captures to a log file and serves the live view. Add `--dir <path>` to change where captures are saved |
| `candeck sniff` | Opens the live view |
| `candeck mark start\|stop <label>` | Marks an action from another terminal or a script |
| `candeck compare [capture]` | Shows what changed during marked actions, and likely senders |
| `candeck senders [capture]` | Shows likely senders only |
| `candeck snapshot` | Prints the live view once as plain text. Add `--secs <seconds>` to change how long it collects first |
| `candeck sim [iface]` | Generates simulated traffic on a `vcan` interface |

## Capture files

Captures are saved in `~/.local/share/candeck/captures/`, named by start time and interface:

- `<time>-<iface>.log` is the traffic in standard candump format, so it works with `can-utils` and other tools.
- `<time>-<iface>.markers` holds your marked actions, one per line: `<microseconds> start|stop <label>`.

Data is written to disk about once a second, so a power loss costs about a second of capture.

## Trying it without hardware

Create a virtual interface and generate simulated traffic. The IDs from `sim` are invented and don't represent any vehicle.

```
sudo modprobe vcan
sudo ip link add vcan0 type vcan
sudo ip link set vcan0 up

candeck daemon vcan0
candeck sim vcan0
candeck sniff
```

Run the last three commands in separate terminals. `detect` isn't needed because a virtual interface has no bitrate.

## Troubleshooting

**"checksum did not match" or "download failed" during install**
: Nothing was installed. Check your connection and run it again, or download the archive by hand from the releases page.

**"No can0 found"**
: The adapter isn't detected. Reconnect it and check that it appears in `lsusb` and `ip link`.

**"sudo: a password is required"**
: The sudo rule from the install steps is missing, or the username in it is wrong.

**"no traffic seen at any rate"**
: Nothing was heard on the bus. Make sure the ignition is on and check the wiring. Some modules stay quiet until the car is fully awake, and some vehicles don't pass traffic through the OBD port.

**"no rate matched, but bus errors were seen"**
: Something is on the bus, but it didn't decode cleanly at any rate. Check CAN High and CAN Low aren't swapped and that the connection is solid. The bus may also use a rate that isn't in the list: 1 Mbit/s, 800, 500, 250, 125, 100, 83.3 and 50 kbit/s.

**"kernel did not confirm listen-only"**
: candeck stopped on purpose, before bringing the link up. Your adapter or driver may not support listen-only mode. Don't use this adapter on a live bus with candeck until that's resolved.

**"can0 is not in listen-only mode"**
: The daemon won't capture on a real interface unless it's listen-only. Run `sudo candeck detect can0` first.

**"Is `candeck daemon` running?"**
: `sniff`, `snapshot` and `mark` need the daemon. Start it first.

**"A capture on can0 is already running"**
: The icon opened the live view on the existing capture. To start a new one, stop that daemon first.

**"capture lost" is above zero in the live view**
: Frames were dropped before reaching the log, so treat that capture with caution. Close other heavy programs and capture again. "ui lost" only affects the display, not the log.

**"no actions were marked" or "Nothing changed consistently"**
: Check that the top line showed `MARKING` during your test. Otherwise, try a longer block with several firm repeats and a few quiet seconds on each side.

**A signal you expected isn't listed**
: Gradual values appear under the bytes section, not as pulses. Some signals may also be on a different bus from the one you're connected to.

**"Capture too short" under Likely senders**
: Record for at least 40 seconds, or about 90 seconds to group slow IDs.
