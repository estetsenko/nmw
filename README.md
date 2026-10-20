# NMW

A mesh networking protocol for LoRa radios (Heltec V4 first) with text, voice
notes and data, phone/PC companion apps, bots and internet gateways.

The design is in [docs/architecture.md](docs/architecture.md).

## Layout

| Crate | What it is |
|---|---|
| `crates/nmw-core` | The protocol: packet codec, identities, ChaCha20-Poly1305, routing and reliable delivery. `no_std` + `alloc`, sans-IO, shared by firmware and apps. |
| `crates/nmw-sim` | Deterministic multi-node simulator (airtime, collisions with capture, listen-before-talk, random loss). |

## Build and test

```sh
cargo test --all
cargo run --release -p nmw-sim -- --help
cargo run --release -p nmw-sim -- --seeds 20 --shared-contacts --loss 0.2 --min-delivery 0.95
```

## Status

M0 (protocol core + simulator). Next: Heltec V4 firmware (M1), then the desktop app.
