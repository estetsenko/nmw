# New Mesh World: Protocol & App Architecture (draft v0.4)

Status: draft v0.4, 2026-10-20 (§4–§7 updated to match the M0 implementation in `nmw-core`). Decided: Heltec V4, desktop app first, EU 868, balanced `medium` profile, Rust firmware, voice notes before live voice, new protocol (not built on Reticulum/MeshCore). Working name for the protocol: **NMW**.
Remaining **[default]** marks are implementation choices that can change without affecting the design; §16 records the decisions.

## 1. Goals

| Goal | What it means concretely |
|---|---|
| Text | 1:1 and group messages, delivery receipts, store-and-forward for offline peers |
| Voice | (a) voice notes sent as data, works everywhere; (b) live push-to-talk on short, fast links |
| Data | Arbitrary payloads: files up to ~64 KB, sensor/telemetry, position, app-defined ports |
| Portable | Battery device (pocket/keychain size), works standalone with a screen or paired to a host |
| Phone/PC | Same host protocol over BLE, USB serial, and Wi-Fi/TCP |
| Bots | Any node can be a bot; SDK for writing bots on a PC/server attached to a node or via a gateway |
| Gateways | Bridge mesh to internet: mesh-to-mesh federation and mesh-to-internet services, without breaking end-to-end encryption |

Non-goals for v1: IP-over-mesh (TCP/IP tunnelling), high-throughput video, anonymity against a global passive adversary.

## 2. Hardware assumptions [default]

- **Radio:** Semtech SX1262 (sub-GHz LoRa + GFSK). Optional later: LR1121 (adds 2.4 GHz LoRa, faster local links).
- **Reference board (decided): Heltec WiFi LoRa 32 V4** (ESP32-S3 + SX1262, BLE + Wi-Fi, USB-C, OLED). One board covers handheld, repeater, gateway and bot host, so v1 has a single firmware target.
  - Wi-Fi on the same board means any node can act as a gateway, and the desktop app can connect over USB or Wi-Fi.
  - Trade-off: ESP32-S3 draws more power than nRF52840, so battery life for an always-listening handheld is hours-to-days, not weeks. Firmware should use light sleep with radio RX duty-cycling for `client` role.
  - Check the V4's front-end module (external PA/LNA) power limits against the region profile; firmware must cap TX power per region.
- Later targets (same core, new board support): nRF52840 boards (RAK4631) for low-power handhelds, LilyGO T-Deck for a standalone keyboard device.
- **Bands:** EU 863–870 MHz (duty-cycle limited: 1% in most sub-bands, 10% at 869.40–869.65), US 902–928 MHz. Region profile is a mandatory config item; firmware enforces duty-cycle and power limits.
- **First region (decided): EU 868.** Primary mesh channel sits in the 869.40–869.65 MHz sub-band (10% duty cycle, up to 500 mW ERP), e.g. 869.525 MHz with BW250. Firmware tracks airtime per rolling hour and refuses to transmit past the limit; the desktop app shows the remaining budget. US 915 is added later as a second region profile.

### Radio profiles

The airtime budget drives almost every other decision. Approximate LoRa bitrates:

| Profile | Modulation | Raw rate | Use |
|---|---|---|---|
| `long` | LoRa SF11 / BW250 | ~1.1 kbps | Default mesh, km-scale hops |
| `medium` (decided) | LoRa SF9 / BW250 | ~3.5 kbps | Urban mesh, good balance; the v1 primary profile |
| `fast` | LoRa SF7 / BW500 | ~22 kbps | Short hops, live voice |
| `burst` | GFSK 100 kbps | ~100 kbps | Same-room/campus file and voice transfer |

A mesh runs on one primary profile (all repeaters agree). `fast`/`burst` are negotiated per link for bulk transfers: the two nodes agree on the primary channel, hop to a secondary channel/profile, transfer, and return.

## 3. Layer model

```
+-------------------------------------------------------------+
| Apps: chat, voice, files, telemetry, bots, gateway services |
+-------------------------------------------------------------+
| Ports/services (16-bit port numbers, like UDP)              |
+-------------------------------------------------------------+
| Transport: datagram | reliable (ACK/retry) | resource (frag)|
+-------------------------------------------------------------+
| Session crypto: identities, ECDH, AEAD, group keys          |
+-------------------------------------------------------------+
| Mesh: addressing, announces, flooding + source routing      |
+-------------------------------------------------------------+
| Link: framing, MAC (listen-before-talk, priority), dup cache|
+-------------------------------------------------------------+
| PHY: SX1262 LoRa/GFSK profiles, region limits               |
+-------------------------------------------------------------+
```

The **same core library** implements Link → Transport and is shared by firmware, simulator, desktop, and mobile (see §11).

## 4. Identity and addressing

- **Node identity:** one Ed25519 key pair from a 32-byte seed. The X25519 key-agreement key is derived from it (the libsodium `ed25519_pk_to_curve25519` mapping), so an identity is a single 32-byte public key.
- **Node address (full):** first 16 bytes of `SHA-256(public key)`. Used in contacts, QR codes and on the internet side.
- **On-air address (short):** first **4 bytes** of the full address. Collisions are resolved by the full address in announces and the AEAD tag failing for the wrong recipient; at 4 bytes the collision risk is negligible below ~10k nodes per mesh.
- **Channels (groups):** identified on air by a 4-byte id, `SHA-256("nmw-v0 channel id\0" | key)[0..4]`, carried in `dst`, plus the channel name locally. A public channel (`#name`) derives its key from the lowercased name, so anyone who knows the name can join; a private channel has a random 32-byte key shared as an invite.
- **Services:** a node exposes ports; well-known ports: 1 text, 2 receipts, 3 voice note, 4 live voice, 5 file/resource, 6 position, 7 telemetry, 8 bot command, 9 gateway control, 100+ app-defined.
- **Human names** are just signed fields in announces; never trusted for identity. Contacts are pinned by full address (TOFU + optional QR verification).
- **Contacts** come from announces, from a contact request, or are shared out of band (QR code / contact file). A sender that has not yet heard back from a contact uses `DATA_KEY` (see §5), so the receiver can decrypt even if it never heard the sender's announce.
- **Contact request:** sending to a full address with no known key floods a `CTRL` contact request (`subtype 1 | full address(16)`); the target answers with its announce, source-routed back along the request's path, at most once per 10 s per requester. The message waits and goes out as soon as the announce arrives; up to 4 requests before the send fails.

## 5. Packet format

LoRa max payload is 255 bytes; every header byte costs airtime on every hop. The fixed header is 14 bytes.

```
Byte   Field
0      ver(2b) | type(3b) | routed(1b) | has_path(1b) | ack_req(1b)
1      hop_limit(4b) | hop_count(4b)
2-5    packet_id (32-bit per-source counter; with src = dedup key and AEAD nonce)
6-9    dst (4-byte short address; FF FF FF FF = broadcast)
10-13  src (4-byte short address)
[14]   path_len (if has_path), then path_len x 1-byte repeater tags (max 15)
...    payload
```

- **packet_id is 32-bit** (not 16) because it forms the AEAD nonce with `src`; a 16-bit counter would wrap within days on a busy node. Firmware persists the counter (or restarts it from a random value) across reboots.
- **AAD** = version, type, routed, ack_req, packet_id, dst, src. The hop counters and path are excluded because repeaters change them.
- **Types:** `DATA` (0), `ACK` (1), `ANNOUNCE` (2, flooded, or source-routed when answering a contact request), `GROUP` (3, flooded to a channel id, not acknowledged), `CTRL` (4: subtype 1 = contact request, 2 = whois by short address), `DATA_KEY` (5: DATA prefixed with the sender's 32-byte public key).
- **Payloads:**
  - `DATA`: AEAD(`seq(2) | port(2) | data`) + 8-byte tag. `seq` is the end-to-end message number used for ACKs and duplicate suppression; every (re)transmission gets a fresh packet_id. Max application data: 181 bytes.
  - `GROUP`: AEAD under the channel key of `port(2) | sig(64) | data`, where `sig` is the sender's Ed25519 signature over `"nmwG" | channel_id | src | packet_id | port | data`. Receivers verify it against the sender's key from their signed announce and show the announced name, so a member cannot post as someone else. A post from a sender whose announce we lack is held for up to 20 s while we flood a whois (`CTRL` subtype 2, `dst` = sender's short address; the owner answers with a flooded announce, at most once a minute, and members that overhear a whois don't send their own). If no announce arrives the post is shown marked unverified. Max application data: 151 bytes.
  - `ACK`: AEAD(`seq(2) | has_fwd_path(1) | fwd_path`). When the DATA arrived by flood, the ACK carries the path it took so the sender learns the route.
  - `ANNOUNCE`: `pubkey(32) | role(1) | name_len(1) | name(≤32) | sig(64)`, signed over `"nmwA" | packet_id | body`. About 130 bytes.
- **Fragmentation** lives in transport (resource packets carry `resource_id(2) | frag_idx(1) | frag_count(1)`), not in the base header.
- **Repeater tag:** 1 byte = first byte of a repeater's address, as in MeshCore. Ambiguity is tolerable because it is only a forwarding hint; the end-to-end AEAD protects integrity.

## 6. Routing

Hybrid of managed flooding and learned source routes. The two existing approaches each fail somewhere: pure flooding (Meshtastic) wastes airtime as the mesh grows; pure source routing needs discovery first.

1. **Roles** (announced, configurable):
   - `client`: never repeats (phones' companion devices, handhelds). Default for portable devices.
   - `repeater`: repeats floods and routed packets. Fixed, well-placed nodes.
   - `room`: repeater + store-and-forward mailbox for offline clients and group history.
   - `gateway`: repeater/room + internet uplink.
2. **Announces:** each node periodically floods a signed announce (address, pubkeys, name, role, capabilities, battery/position optional). Repeaters rate-limit and cache announces; an announce is re-flooded at most once per interval per origin.
3. **Flood forwarding:** repeaters wait a random delay weighted by SNR (weaker SNR = rebroadcast sooner, because that node is likely further away and covers new ground), and cancel if they hear the same packet rebroadcast by enough others. Dedup cache keyed on `(src, packet_id)`.
4. **Path learning:** when a flooded packet arrives, the receiver records the path it took (repeaters append their tag) and answers along the reversed path; its ACK carries the forward path so the sender learns it too. Announces also give every listener a route back to the announcer. Subsequent traffic is source-routed and only the listed repeaters forward.
5. **Hop-by-hop reliability on routes:** after sending a routed packet, a node listens for the next repeater forwarding it (an implicit ACK; on the last hop of a DATA packet, for any packet from the destination) and retransmits up to 2 times. The last hop of an ACK has no such signal, so it is sent twice. A repeater that overhears a routed packet before its turn ignores it rather than marking it seen.
6. **Path repair:** if a routed message fails twice end-to-end, or its first hop dies, the sender drops the route and floods; the reply re-establishes a route. Up to 3 end-to-end retries in total.
7. **Hop limit:** default 6 repeaters, max 15. Live voice is capped at 2 hops.

## 7. Cryptography

- **Primitives:** X25519, Ed25519, HKDF-SHA256, ChaCha20-Poly1305 (fast in software on both MCUs; AES-CCM would use the nRF52 hardware block but adds a second code path, so not in v1).
- **1:1:** static-static ECDH → HKDF → per-pair key; nonce = `src | packet_id | type`. Cheap, zero handshake airtime. Optional **session ratchet** (a Noise IK handshake, ~2 packets) for forward secrecy when both peers are online and the link is good.
- **Groups/channels:** symmetric channel key shared by QR/link (or derived from a public `#name`); every post carries the sender's Ed25519 signature (+64 bytes), so the key only grants reading and posting, not impersonation.
- **Announces** are signed; repeaters verify before re-flooding (cuts spoofed-announce floods).
- **Truncated tag:** 8-byte Poly1305 tag on air. Acceptable for this threat model; packets with failed tags are dropped silently.
- **Headers are not encrypted** (repeaters need dst/path). This leaks who talks to whom to local listeners; documented as a known limitation.
- **Key storage:** keys live on the radio device (works standalone without a phone), with encrypted export/backup to the host app.

## 8. Transport and services

| Mode | Behaviour | Used by |
|---|---|---|
| Datagram | Fire and forget | Position, telemetry, live voice |
| Reliable | End-to-end ACK, up to 3 retries with backoff, receipts | Text, bot commands |
| Resource | Fragmented, windowed (window 4), selective ACK bitmap, resumable | Files, voice notes, images |

- **Priority queue at the MAC:** ACK > live voice > text > control > resources > announces.
- **Store-and-forward:** `room` and `gateway` nodes hold messages for clients that announced but are asleep or out of range; clients poll on wake ("anything for me?").
- **Message payloads** use a compact binary encoding (CBOR) [default] so apps can add fields without breaking older nodes.

## 9. Voice

Two modes, because live voice over a multi-hop LoRa mesh is the hardest part and should not block the rest.

### 9.1 Voice notes (v1)
- Record on phone or device, encode with **Codec2 1200 bps** (or 700C for `long` profile), send as a resource on port 3.
- 10 s of speech at 1200 bps ≈ 1.5 KB ≈ 7 packets. Works on every profile and every hop count, survives loss via retries.
- Optional: Opus at 6 kbps when the transfer runs over the `burst` profile or a gateway.

### 9.2 Live push-to-talk (v2, decided: after voice notes)
- Half-duplex PTT on port 4, Codec2 1200/1600 bps, 40 ms frames.
- Bundle 8 frames (320 ms, ~48–64 bytes) per packet; no retransmission; receiver jitter buffer ~600 ms; lost packet = 320 ms gap with comfort noise.
- Requires `fast` profile or better and ≤ 2 hops. Before a talk-spurt the sender negotiates the fast profile with the target/group (CTRL packet).
- **Regulatory caveat:** in EU 1% duty-cycle sub-bands this allows only ~36 s of transmission per hour per device; live voice in EU realistically needs the 10% sub-band (869.4–869.65) or the 2.4 GHz LR1121 path. US 915 has no duty cycle for wideband (BW500) digital modulation, but confirm locally.

## 10. Phone / PC link (host protocol)

The device is a **companion radio** (like MeshCore companions / Meshtastic clients) and can also run standalone.

- **Transports:**
  - BLE GATT: one service, `RX` (write without response) and `TX` (notify) characteristics, MTU-negotiated, frames chunked.
  - USB CDC-ACM serial.
  - Wi-Fi TCP (ESP32 only), port 4403-style; also how a gateway host talks to its radio.
- **Framing:** `0xNM | len(2) | protobuf message`. Same messages over every transport.
- **Protocol buffers** [default] on the host link (not on air): good codegen for Kotlin, Swift, Dart, Python, Rust, TypeScript; bandwidth is not scarce here.
- **Message families:** `Config` (region, profile, role, name), `Contacts`, `Channels`, `Send` (port, dst, payload, reliability), `Receive`, `Status` (battery, airtime used, neighbours), `Log`, `Firmware update`.
- **Pairing:** BLE with passkey shown on device screen (or fixed PIN for screenless devices), and host apps authenticate to the device with a host key so a random phone cannot drive someone else's radio.

## 11. Software architecture

```
          +------------------- nmw-core (Rust, no_std) -------------------+
          | packet codec | routing | crypto | transport | host protocol    |
          +-------+-------------+----------------+-------------+----------+
                  |             |                |             |
        firmware (Embassy)   simulator       nmw-host lib    gateway
        Heltec V4 (ESP32-S3) (many nodes,    (std, async)    daemon
                             virtual radio)       |
                                     +------------+-------------+
                                  mobile app    desktop app    bot SDK
                               (Kotlin/Swift   (Tauri)        (Python via
                                via UniFFI, or               PyO3, Rust,
                                Flutter)                     TypeScript)
```

- **Why Rust core:** one implementation of the protocol everywhere (firmware, apps, gateway, simulator) means no drift between implementations, and memory safety on the parsing path that faces the radio. Embassy (via `esp-hal`) supports ESP32-S3 for the Heltec V4, and nRF52840 later.
- **Simulator first:** a deterministic, multi-node simulator with a configurable loss/latency/airtime model lets us test routing and transport before touching hardware, and runs in CI.
- **Firmware language (decided): Rust** (Embassy + `esp-hal`) for the Heltec V4.
- **Radio driver:** Rust `lora-phy` crate for SX126x; fall back to wrapping RadioLib (C++) if it lacks something.

### 11.1 Desktop app (first client)

- **Stack:** Tauri (Rust backend + web UI). The backend links `nmw-host` directly, so there is no second protocol implementation and no extra daemon.
- **Connection:** USB serial (auto-detects the V4) and Wi-Fi TCP to a V4 on the LAN; BLE from desktop is deferred.
- **v1 features:** contacts and 1:1 chat, channels, delivery receipts, node configuration (region, profile, role, name), live neighbour/route view, airtime and battery status, firmware update over USB.
- **Doubles as dev tooling:** a packet log/inspector and a mode that drives the simulator instead of real hardware, so the same UI is used for protocol debugging.
- **Bot hosting:** the desktop app can run bots locally against its attached radio, which makes it the easiest bot host for M4.
- **Built so far (2026-10-20):** `apps/desktop` links `nmw-host`, whose `Mesh` trait is what the UI talks to. The only backend today is `VirtualMesh`: your node inside a simulated 20-node mesh (6 repeaters, 10% loss) with an Echo bot and an Info bot, run in real time on a background thread. The UI has contacts with live route length, 1:1 chat with delivery receipts and a 181-byte counter, opening a chat by address, a manual announce and the radio log. A serial backend for a real V4 slots in behind the same trait.

## 12. Bots

A bot is an ordinary node identity plus code. Three places it can run:

1. **On a PC/Pi attached to a radio** (USB/TCP) using the bot SDK. Default and easiest.
2. **Behind a gateway**, with no radio of its own: it registers an identity with a gateway, which announces it on the mesh.
3. **On the device firmware** (small built-in bots: ping/echo, weather station, repeater status).

Bot SDK sketch (Python):

```python
from nmw import Bot

bot = Bot.connect("serial:///dev/ttyACM0", name="WeatherBot")

@bot.command("weather")
async def weather(msg, city: str = "here"):
    return f"{city}: 12C, light rain"

@bot.on_message
async def any_message(msg):
    if msg.channel == "ops":
        await bot.store(msg)

bot.run()
```

- **Conventions:** commands are text messages starting with `/`; bots announce `capabilities: ["bot"]` plus a command list so apps can show buttons/autocomplete.
- **Structured replies:** port 8 carries CBOR `{cmd, args}` / `{result, buttons}` for apps that support rich bot UIs; text fallback always works.
- **Airtime guard:** the SDK enforces per-bot rate limits and reply size caps, because a chatty bot can saturate a mesh.

## 13. Internet gateways

Gateway = ESP32 node or a radio attached to a Linux box running the gateway daemon.

- **Uplink:** QUIC (or WebSocket fallback) to one or more **relay servers**; MQTT bridge as an optional integration for home-automation users.
- **What crosses the gateway:**
  - Unicast packets whose destination is known to be reachable via the internet (learned from relay announces). Payloads stay end-to-end encrypted; the gateway only sees headers.
  - Channels the gateway operator explicitly bridges.
  - **Never** blind re-injection of all internet traffic into the radio mesh (the classic MQTT-flood problem). Gateways have an airtime budget for internet-originated packets.
- **Federation:** two meshes in different cities talk via relays; addresses are global (16-byte full address), so a contact works in either place.
- **Internet services** are just bots behind a gateway: email-to-mesh, Telegram/Matrix bridges, weather, SOS forwarding.
- **Internet clients:** a phone without a radio can join through a relay as a normal identity, so the same app works with or without hardware.

## 14. Comparison with existing projects

| | Meshtastic | MeshCore | Reticulum | NMW (this design) |
|---|---|---|---|---|
| Routing | Managed flood (+ next-hop for DMs in 2.6) | Flood to discover, then source routes via fixed repeaters | Announce-based, destination-hash routing, transport nodes | Flood-to-discover + source routes, role-based repeaters, store-and-forward rooms |
| Clients repeat? | Yes by default | No (repeaters only) | Configurable | No by default |
| Addressing | 4-byte node num | Pubkey prefix, 1-byte path hashes | 16-byte destination hash | 4-byte on air, 16-byte full, 1-byte path tags |
| Crypto | AES-CTR channel PSK; PKC for DMs (2.5+) | Ed25519/X25519 per contact | X25519/Ed25519, ratchets, strong | X25519/Ed25519, AEAD, optional ratchet |
| Voice | Not really | No | Via LXST (experimental, needs bandwidth) | Voice notes v1, PTT on fast links v2 |
| Medium | LoRa only | LoRa only | Any (LoRa, TCP, serial, packet radio) | LoRa/GFSK primary, IP via gateways |
| Implementation | C++ firmware, many apps | C++ firmware | Python (heavy for MCU) | Rust core shared by firmware and apps |

**What we take:** Meshtastic's SNR-weighted rebroadcast and broad hardware support; MeshCore's non-repeating clients and path tags; Reticulum's identity-hash addressing and medium independence.
**Where we differ:** a single shared core across firmware and apps, voice as a first-class service, a bot SDK and gateway airtime policy designed in from the start.

**Alternative worth considering:** build NMW apps on top of Reticulum or MeshCore instead of a new protocol. Faster to ship, but we'd inherit Reticulum's Python-centric stack (hard to run on nRF52) or MeshCore's narrower scope. Recommended only if speed to a first usable app matters more than owning the protocol.

### Simulator results (2026-10-20)

`nmw-sim`: 50 nodes (15 repeaters, 35 clients) in 6×6 km, 2 km range, SF9/BW250, collisions with 6 dB capture, listen-before-talk; 300 messages per run at one every 10 s, 20 random topologies.

| Per-link loss | Delivered, contacts from announces + contact requests | Delivered, contacts shared by QR | p95 latency (announces) |
|---|---|---|---|
| 0% | 99.6% | 100% | 1.5 s |
| 20% | 98.5% | 99.2% | 13 s |
| 30% | 94.7% | 97.3% | 45 s |

At 0% loss every failure is a pair more than 7 radio hops apart, beyond the 6-repeater hop limit. Contact requests lifted announce-only delivery at 20% loss from 91% to 98.5%.

**Group channels** (`nmw-sim --group`: all 35 clients in one channel, 300 posts; delivery counts post × other member pairs):

| Per-link loss | Delivered | Sender verified | p95 latency |
|---|---|---|---|
| 0% | 99.4% | 98.9% | 4.2 s |
| 20% | 95.1% | 97.5% | 6.2 s |
| 30% | 91.3% | 93.4% | 23 s |

Group posts have no end-to-end ACK, so whoever sends or rebroadcasts one listens for a repeater further out passing it on and sends it once more if none does (`FLOOD_ECHO_RETRIES = 1`), and a pending rebroadcast is only cancelled after hearing 3 copies (`group_redundancy`). Together these lifted delivery at 20% loss from 87.3% to 95.6%, at the cost of the busiest node's airtime rising from 1.9% to 2.9% under this heavy load. A second echo retry would add only 0.6 points for another 1.1 points of airtime. Signing posts makes each frame about 55 bytes longer, which raises the busiest node to about 5% airtime under this load (a post every 10 s), and posts from senders not yet known wait for the whois answer, which shows in the higher p95 latency.

## 15. Roadmap

| Milestone | Scope | Exit criteria |
|---|---|---|
| M0 | `nmw-core` codec + crypto + simulator | 50-node simulated mesh delivers 1:1 text with > 95% success at 20% loss |
| M1 | Firmware on Heltec V4, USB host link, CLI client | Two V4 nodes exchange encrypted text |
| M2 | Routing on hardware, repeaters, announces | 5+ node field test across a few km |
| M3 | Desktop app (Tauri: Windows, macOS, Linux) over USB serial and Wi-Fi TCP | Chat, contacts, channels, node config from a PC |
| M3b | Mobile app over BLE (after desktop) | Same features from a phone |
| M4 | Gateway daemon + relay server + bot SDK (Python) | Two meshes federated; a bot answers commands |
| M5 | Voice notes; resource transfer; `burst` profile | 10 s voice note across 3 hops |
| M6 | Live PTT on `fast` profile | Intelligible PTT across 2 hops |

## 16. Open questions (answers change the design)

1. ~~Target hardware~~ **Decided:** Heltec V4.
2. ~~Region~~ **Decided:** EU 868 first (869.525 MHz, 10% sub-band); US 915 later.
3. ~~Range vs bandwidth~~ **Decided:** balanced, `medium` profile (SF9/BW250).
4. ~~Languages~~ **Decided:** Rust for core and firmware.
5. ~~Mobile vs desktop~~ **Decided:** desktop app first; mobile later as native apps (Kotlin first, then Swift) over UniFFI.
6. ~~New protocol vs Reticulum/MeshCore~~ **Decided:** new protocol.
7. ~~Live voice~~ **Decided:** voice notes in v1 (M5); live PTT in M6.

No open design questions remain for v1. Next step: M0, the `nmw-core` crate and simulator.
