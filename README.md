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

## Status

M0 (protocol core + simulator) is done. The desktop app works against the simulated mesh; Heltec V4 firmware (M1) is next once hardware is available, and then the app switches to a real radio over USB.
