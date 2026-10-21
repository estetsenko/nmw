# NMW

A mesh networking protocol for LoRa radios (Heltec V4 first) with text, voice
notes and data, phone/PC companion apps, bots and internet gateways.

The design is in [docs/architecture.md](docs/architecture.md).

## Layout

| Crate | What it is |
|---|---|
| `crates/nmw-core` | The protocol: packet codec, identities, ChaCha20-Poly1305, routing and reliable delivery. `no_std` + `alloc`, sans-IO, shared by firmware and apps. |
| `crates/nmw-sim` | Deterministic multi-node simulator (airtime, collisions with capture, listen-before-talk, random loss). |
| `crates/nmw-host` | Host-side runtime for apps, bots and gateways. Today it drives a simulated mesh with an Echo bot and an Info bot; a radio over USB plugs in behind the same `Mesh` trait. |
| `crates/nmw-device` | Board-independent device logic: SX1262 driver, GPS (NMEA) reader, message inbox and the 128x64 screens. `no_std`, tested on the host. |
| `firmware/heltec-v4` | Firmware for the Heltec V4: text messages and GPS on the OLED. Builds with Espressif's Rust toolchain, outside the workspace; see [its README](firmware/heltec-v4/README.md). |
| `apps/desktop` | Tauri desktop app (Rust backend in `src-tauri`, plain HTML/JS UI in `ui`). |

## Build and test

```sh
cargo test --all
cargo run --release -p nmw-sim -- --help
cargo run --release -p nmw-sim -- --seeds 20 --shared-contacts --loss 0.2 --min-delivery 0.95
```

## Desktop app

The desktop app runs against a simulated 20-node mesh until the Heltec V4
firmware exists. On Linux it needs WebKitGTK
(`libwebkit2gtk-4.1-dev librsvg2-dev libayatana-appindicator3-dev`); on
macOS and Windows nothing extra.

```sh
cargo run -p nmw-desktop            # mesh time runs at real speed
NMW_SPEED=4 cargo run -p nmw-desktop  # 4x faster, handy for testing
```

Installable packages (`.deb`, `.rpm` and `.AppImage` for Linux, a universal
`.dmg` for macOS, `.msi` and an `.exe` installer for Windows) are built by the
CI workflow on every push and PR, after the tests pass; download them from the
run's artifacts. Every push to main also refreshes the public
[Latest build](https://github.com/estetsenko/nmw/releases/tag/latest-build)
pre-release, and pushing a `v*` tag attaches them to a draft release.
They are not code-signed yet, so macOS and Windows will warn on first launch.
To build one locally: `cargo install tauri-cli --version '^2'`, then
`cargo tauri build` in `apps/desktop/src-tauri`.

Your node's identity is kept in `identity.key` in the app data directory, so
the address stays the same between runs. Try **Echo bot** (repeats what you
send) and **Info bot** (`/help`, `/ping`, `/hops`, `/time`, `/nodes`). The
**Radio log** button shows every frame your node sent or heard.

Channels: the app joins **#public** on first run, where simulated users chat
and Info bot answers commands. Join any `#name` to use a public channel
(anyone who knows the name can read it). Type a name without `#` to create a
private channel; **Copy invite key** gives others the key to join it.
Every post is signed by its sender, so names in a channel can't be faked; a
post whose sender the app can't check yet is marked unverified. Joined channels are saved in `channels.json` next to `identity.key`.

Long messages are sent as up to 16 packets and shown whole on the other side,
so a message can be up to 2832 bytes (2352 in a channel); the byte counter
shows how many parts it will take.

The paperclip button sends a photo or file (up to 32 KB) to a contact. Photos
are shrunk to about 12 KB first, since each kilobyte takes roughly 10 seconds
per radio hop. Echo bot sends files back, and received files can be saved
wherever you choose.

## Status

M0 (protocol core + simulator) is done. The desktop app works against the simulated mesh. The first Heltec V4 firmware (M1) shows #public and direct messages and the GPS fix on the board's screen and shares positions; it is untested on hardware. Next: a USB link so the desktop app can use the board as its radio.

To preview the board's screens without hardware: `cargo run -p nmw-device --example screens -- /tmp/screens`.
