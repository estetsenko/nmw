//! Mesh node state machine (sans-IO).
//!
//! The caller owns the radio and the clock:
//! - feed every received frame into [`Node::handle_frame`],
//! - call [`Node::tick`] at [`Node::next_wakeup`] (or more often),
//! - transmit whatever [`Node::poll_transmit`] returns (after listen-before-talk),
//! - drain [`Node::poll_event`] for the application.
//!
//! Routing: packets to a destination without a known route are flooded;
//! repeaters append their tag to the path. The receiver answers along the
//! reversed path and returns the forward path in its ACK, so both ends learn
//! a source route. Routes that fail twice fall back to flooding.

use crate::airtime::LoraProfile;
use crate::crypto;
use crate::identity::{Address, Identity, PublicIdentity, ShortAddr};
use crate::packet::{Packet, PacketType, HEADER_LEN, MAX_FRAME, MAX_PATH};
use crate::rng::Rng;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

/// Time in microseconds, from any fixed origin.
pub type Micros = u64;

const SEEN_TTL: Micros = 120_000_000;
const DELIVERED_TTL: Micros = 600_000_000;
const PRUNE_EVERY: Micros = 10_000_000;
const TIMEOUT_MARGIN: Micros = 2_000_000;
const MAX_NAME: usize = 32;
/// Ciphertext overhead of a DATA payload: seq(2) + port(2) + tag.
const DATA_OVERHEAD: usize = 4 + crypto::TAG_LEN;

const PRIO_ACK: u8 = 0;
const PRIO_FORWARD: u8 = 1;
const PRIO_DATA: u8 = 2;
const PRIO_ANNOUNCE: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Never repeats. Default for portable devices.
    Client = 0,
    /// Repeats floods and source-routed packets.
    Repeater = 1,
}

impl Role {
    fn from_byte(b: u8) -> Self {
        if b == Role::Repeater as u8 {
            Role::Repeater
        } else {
            Role::Client
        }
    }
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub role: Role,
    pub name: String,
    pub profile: LoraProfile,
    pub hop_limit: u8,
    /// Retransmissions after the first attempt before reporting failure.
    pub max_retries: u8,
    /// A pending flood rebroadcast is cancelled after hearing this many other copies.
    pub redundancy: u8,
    /// Seed for timing jitter (take it from a hardware RNG on devices).
    pub rng_seed: u64,
}

impl NodeConfig {
    pub fn new(role: Role, name: &str) -> Self {
        NodeConfig {
            role,
            name: name.into(),
            profile: LoraProfile::MEDIUM,
            hop_limit: 6,
            max_retries: 3,
            redundancy: 2,
            rng_seed: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A new contact was learned from its announce.
    Contact {
        addr: Address,
        name: String,
        role: Role,
    },
    /// A message addressed to this node (delivered once per sender sequence number).
    Message {
        from: Address,
        port: u16,
        seq: u16,
        data: Vec<u8>,
    },
    /// The destination acknowledged message `seq`.
    Delivered { to: Address, seq: u16 },
    /// No acknowledgement after all retries.
    Failed { to: Address, seq: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// No announce has been received from the destination yet.
    UnknownContact,
    TooLarge,
}

struct Contact {
    addr: Address,
    name: String,
    role: Role,
    key: [u8; 32],
    /// Repeater tags from this node to the contact; empty means direct neighbour.
    route: Option<Vec<u8>>,
}

struct Queued {
    at: Micros,
    prio: u8,
    frame: Vec<u8>,
    /// Set for flood rebroadcasts, which are cancelled when enough copies are overheard.
    flood_key: Option<(ShortAddr, u32)>,
    heard: u8,
}

struct Outgoing {
    to: ShortAddr,
    seq: u16,
    port: u16,
    data: Vec<u8>,
    attempts: u8,
    routed_attempts: u8,
    last_routed: bool,
    deadline: Micros,
}

pub struct Node {
    identity: Identity,
    me: ShortAddr,
    config: NodeConfig,
    rng: Rng,
    contacts: BTreeMap<ShortAddr, Contact>,
    seen: BTreeMap<(ShortAddr, u32), Micros>,
    delivered: BTreeMap<(ShortAddr, u16), Micros>,
    queue: Vec<Queued>,
    outgoing: Vec<Outgoing>,
    events: VecDeque<Event>,
    next_id: u32,
    next_seq: u16,
    last_prune: Micros,
}

impl Node {
    /// Firmware must persist the packet id counter across reboots (or start it
    /// from a fresh random value) so nonces are never reused.
    pub fn new(identity: Identity, config: NodeConfig, now: Micros) -> Self {
        let addr = identity.address();
        let mut seed_bytes = [0u8; 8];
        seed_bytes.copy_from_slice(&addr.0[..8]);
        let mut rng = Rng::new(config.rng_seed ^ u64::from_le_bytes(seed_bytes));
        let next_id = rng.next_u64() as u32;
        let next_seq = rng.next_u64() as u16;
        Node {
            me: addr.short(),
            identity,
            config,
            rng,
            contacts: BTreeMap::new(),
            seen: BTreeMap::new(),
            delivered: BTreeMap::new(),
            queue: Vec::new(),
            outgoing: Vec::new(),
            events: VecDeque::new(),
            next_id,
            next_seq,
            last_prune: now,
        }
    }

    pub fn address(&self) -> Address {
        self.identity.address()
    }

    pub fn role(&self) -> Role {
        self.config.role
    }

    /// Known contacts as (address, name).
    pub fn contacts(&self) -> impl Iterator<Item = (Address, &str)> {
        self.contacts.values().map(|c| (c.addr, c.name.as_str()))
    }

    pub fn knows(&self, addr: &Address) -> bool {
        self.contact(addr).is_some()
    }

    /// The learned source route to `addr`, if any (empty = direct neighbour).
    pub fn route_to(&self, addr: &Address) -> Option<&[u8]> {
        self.contact(addr)?.route.as_deref()
    }

    /// Number of messages waiting for an acknowledgement.
    pub fn pending_messages(&self) -> usize {
        self.outgoing.len()
    }

    fn contact(&self, addr: &Address) -> Option<&Contact> {
        self.contacts.get(&addr.short()).filter(|c| c.addr == *addr)
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.seen.insert((self.me, id), Micros::MAX);
        id
    }

    fn airtime(&self, len: usize) -> Micros {
        self.config.profile.airtime_us(len)
    }

    fn enqueue(
        &mut self,
        at: Micros,
        prio: u8,
        frame: Vec<u8>,
        flood_key: Option<(ShortAddr, u32)>,
    ) {
        self.queue.push(Queued {
            at,
            prio,
            frame,
            flood_key,
            heard: 0,
        });
    }

    /// Flood an announce carrying this node's public keys, role and name.
    pub fn announce(&mut self, now: Micros) {
        let id = self.alloc_id();
        let name = self.config.name.as_bytes();
        let name = &name[..name.len().min(MAX_NAME)];
        let mut payload = Vec::with_capacity(PublicIdentity::LEN + 2 + name.len() + 64);
        payload.extend_from_slice(&self.identity.public().to_bytes());
        payload.push(self.config.role as u8);
        payload.push(name.len() as u8);
        payload.extend_from_slice(name);
        let sig = self.identity.sign(&announce_signed_bytes(id, &payload));
        payload.extend_from_slice(&sig);
        let packet = Packet {
            ptype: PacketType::Announce,
            routed: false,
            ack_req: false,
            hop_limit: self.config.hop_limit,
            hop_count: 0,
            id,
            dst: ShortAddr::BROADCAST,
            src: self.me,
            path: Vec::new(),
            payload,
        };
        self.enqueue(now, PRIO_ANNOUNCE, packet.encode(), None);
    }

    /// Send `data` reliably to `to` on `port`. Returns the message sequence number
    /// reported later in [`Event::Delivered`] or [`Event::Failed`].
    pub fn send(
        &mut self,
        to: &Address,
        port: u16,
        data: &[u8],
        now: Micros,
    ) -> Result<u16, SendError> {
        if self.contact(to).is_none() {
            return Err(SendError::UnknownContact);
        }
        if HEADER_LEN + 1 + MAX_PATH + DATA_OVERHEAD + data.len() > MAX_FRAME {
            return Err(SendError::TooLarge);
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.outgoing.push(Outgoing {
            to: to.short(),
            seq,
            port,
            data: data.into(),
            attempts: 0,
            routed_attempts: 0,
            last_routed: false,
            deadline: now,
        });
        let i = self.outgoing.len() - 1;
        self.transmit_outgoing(i, now);
        Ok(seq)
    }

    fn transmit_outgoing(&mut self, i: usize, now: Micros) {
        let (to, seq, port, routed_attempts) = {
            let o = &self.outgoing[i];
            (o.to, o.seq, o.port, o.routed_attempts)
        };
        let Some(contact) = self.contacts.get(&to) else {
            return;
        };
        let key = contact.key;
        let route = if routed_attempts < 2 {
            contact.route.clone()
        } else {
            None
        };
        let routed = route.is_some();
        let hops = route
            .as_ref()
            .map_or(self.config.hop_limit as usize, |r| r.len() + 1);

        let id = self.alloc_id();
        let mut packet = Packet {
            ptype: PacketType::Data,
            routed,
            ack_req: true,
            hop_limit: self.config.hop_limit,
            hop_count: 0,
            id,
            dst: to,
            src: self.me,
            path: route.unwrap_or_default(),
            payload: Vec::new(),
        };
        let mut buf = Vec::with_capacity(4 + self.outgoing[i].data.len() + crypto::TAG_LEN);
        buf.extend_from_slice(&seq.to_be_bytes());
        buf.extend_from_slice(&port.to_be_bytes());
        buf.extend_from_slice(&self.outgoing[i].data);
        crypto::seal(&key, &packet.nonce(), &packet.aad(), &mut buf);
        packet.payload = buf;
        let frame = packet.encode();

        let timeout = self.round_trip_timeout(frame.len(), hops);
        let jitter = self.rng.below(timeout / 4 + 1);
        let o = &mut self.outgoing[i];
        o.attempts += 1;
        o.last_routed = routed;
        if routed {
            o.routed_attempts += 1;
        }
        o.deadline = now + timeout + jitter;
        self.enqueue(now, PRIO_DATA, frame, None);
    }

    /// Worst case for a packet to go `hops` hops and its ACK to come back.
    fn round_trip_timeout(&self, len: usize, hops: usize) -> Micros {
        let air = self.airtime(len);
        let ack_air = self.airtime(HEADER_LEN + 1 + MAX_PATH + 3 + MAX_PATH + crypto::TAG_LEN);
        let per_hop = air + max_flood_delay(air) + ack_air + max_flood_delay(ack_air);
        hops as Micros * per_hop + TIMEOUT_MARGIN
    }

    fn flood_delay(&mut self, snr_db: f32, air: Micros) -> Micros {
        // Weaker SNR -> rebroadcast sooner: that node is likely further away and covers new ground.
        let w = ((snr_db + 20.0) / 30.0).clamp(0.0, 1.0);
        (w * 2.0 * air as f32) as Micros + self.rng.below(air / 2 + 1)
    }

    /// Process a frame received from the radio.
    pub fn handle_frame(&mut self, frame: &[u8], snr_db: f32, now: Micros) {
        let Ok(packet) = Packet::decode(frame) else {
            return;
        };
        if packet.src == self.me {
            return;
        }
        let key = (packet.src, packet.id);
        if self.seen.contains_key(&key) {
            if let Some(pos) = self.queue.iter().position(|q| q.flood_key == Some(key)) {
                self.queue[pos].heard += 1;
                if self.queue[pos].heard >= self.config.redundancy {
                    self.queue.remove(pos);
                }
            }
            return;
        }
        self.seen.insert(key, now + SEEN_TTL);

        let for_me = packet.dst == self.me;
        match packet.ptype {
            PacketType::Announce => {
                self.on_announce(&packet);
                self.forward(packet, snr_db, now);
            }
            PacketType::Data if for_me => self.on_data(&packet, now),
            PacketType::Ack if for_me => self.on_ack(&packet),
            _ => self.forward(packet, snr_db, now),
        }
    }

    fn forward(&mut self, mut packet: Packet, snr_db: f32, now: Micros) {
        if self.config.role != Role::Repeater {
            return;
        }
        if packet.routed {
            let idx = packet.hop_count as usize;
            if idx < packet.path.len() && packet.path[idx] == self.me.tag() {
                packet.hop_count += 1;
                let frame = packet.encode();
                let jitter = self.rng.below(self.airtime(frame.len()) / 4 + 1);
                self.enqueue(now + jitter, PRIO_FORWARD, frame, None);
            }
            return;
        }
        if packet.hop_count >= packet.hop_limit || packet.path.len() >= MAX_PATH {
            return;
        }
        packet.hop_count += 1;
        packet.path.push(self.me.tag());
        let frame = packet.encode();
        if frame.len() > MAX_FRAME {
            return;
        }
        let prio = match packet.ptype {
            PacketType::Ack => PRIO_ACK,
            PacketType::Announce => PRIO_ANNOUNCE,
            _ => PRIO_FORWARD,
        };
        let delay = self.flood_delay(snr_db, self.airtime(frame.len()));
        self.enqueue(now + delay, prio, frame, Some((packet.src, packet.id)));
    }

    fn on_announce(&mut self, packet: &Packet) {
        let p = &packet.payload;
        if p.len() < PublicIdentity::LEN + 2 + 64 {
            return;
        }
        let name_len = p[PublicIdentity::LEN + 1] as usize;
        let body_len = PublicIdentity::LEN + 2 + name_len;
        if p.len() != body_len + 64 {
            return;
        }
        let pub_bytes: &[u8; 64] = p[..64].try_into().expect("length checked");
        let Some(public) = PublicIdentity::from_bytes(pub_bytes) else {
            return;
        };
        let addr = public.address();
        if addr.short() != packet.src {
            return;
        }
        let sig: &[u8; 64] = p[body_len..].try_into().expect("length checked");
        if !public.verify(&announce_signed_bytes(packet.id, &p[..body_len]), sig) {
            return;
        }
        let role = Role::from_byte(p[PublicIdentity::LEN]);
        let name = String::from_utf8_lossy(&p[PublicIdentity::LEN + 2..body_len]).into_owned();
        let route = Some(reversed(&packet.path));

        match self.contacts.get_mut(&packet.src) {
            Some(c) if c.addr == addr => {
                c.name = name;
                c.role = role;
                c.route = route;
            }
            // 4-byte address collision with a different identity: keep the first one.
            Some(_) => {}
            None => {
                let Some(key) = self.identity.pair_key(&public) else {
                    return;
                };
                self.events.push_back(Event::Contact {
                    addr,
                    name: name.clone(),
                    role,
                });
                self.contacts.insert(
                    packet.src,
                    Contact {
                        addr,
                        name,
                        role,
                        key,
                        route,
                    },
                );
            }
        }
    }

    fn on_data(&mut self, packet: &Packet, now: Micros) {
        let Some(contact) = self.contacts.get_mut(&packet.src) else {
            return;
        };
        let mut buf = packet.payload.clone();
        if !crypto::open(&contact.key, &packet.nonce(), &packet.aad(), &mut buf) || buf.len() < 4 {
            return;
        }
        // Either way the packet's path, reversed, leads back to the sender.
        contact.route = Some(reversed(&packet.path));
        let from = contact.addr;
        let seq = u16::from_be_bytes([buf[0], buf[1]]);
        let port = u16::from_be_bytes([buf[2], buf[3]]);

        let duplicate = self.delivered.contains_key(&(packet.src, seq));
        if !duplicate {
            self.delivered
                .insert((packet.src, seq), now + DELIVERED_TTL);
            self.events.push_back(Event::Message {
                from,
                port,
                seq,
                data: buf[4..].to_vec(),
            });
        }
        // Return the forward path of a flooded packet so the sender learns a route.
        let forward_path = if packet.routed {
            None
        } else {
            Some(packet.path.clone())
        };
        // A duplicate means our previous ACK was lost: flood this one.
        self.send_ack(packet.src, seq, forward_path, duplicate, now);
    }

    fn send_ack(
        &mut self,
        to: ShortAddr,
        seq: u16,
        forward_path: Option<Vec<u8>>,
        flood: bool,
        now: Micros,
    ) {
        let Some(contact) = self.contacts.get(&to) else {
            return;
        };
        let key = contact.key;
        let route = if flood { None } else { contact.route.clone() };
        let id = self.alloc_id();
        let mut packet = Packet {
            ptype: PacketType::Ack,
            routed: route.is_some(),
            ack_req: false,
            hop_limit: self.config.hop_limit,
            hop_count: 0,
            id,
            dst: to,
            src: self.me,
            path: route.unwrap_or_default(),
            payload: Vec::new(),
        };
        let mut buf = Vec::with_capacity(3 + MAX_PATH + crypto::TAG_LEN);
        buf.extend_from_slice(&seq.to_be_bytes());
        match forward_path {
            Some(p) => {
                buf.push(1);
                buf.extend_from_slice(&p);
            }
            None => buf.push(0),
        }
        crypto::seal(&key, &packet.nonce(), &packet.aad(), &mut buf);
        packet.payload = buf;
        let frame = packet.encode();
        let jitter = self.rng.below(self.airtime(frame.len()) / 4 + 1);
        self.enqueue(now + jitter, PRIO_ACK, frame, None);
    }

    fn on_ack(&mut self, packet: &Packet) {
        let Some(contact) = self.contacts.get_mut(&packet.src) else {
            return;
        };
        let mut buf = packet.payload.clone();
        if !crypto::open(&contact.key, &packet.nonce(), &packet.aad(), &mut buf) || buf.len() < 3 {
            return;
        }
        let seq = u16::from_be_bytes([buf[0], buf[1]]);
        if buf[2] == 1 && buf.len() - 3 <= MAX_PATH {
            contact.route = Some(buf[3..].to_vec());
        } else if !packet.routed {
            contact.route = Some(reversed(&packet.path));
        }
        let to = contact.addr;
        if let Some(i) = self
            .outgoing
            .iter()
            .position(|o| o.to == packet.src && o.seq == seq)
        {
            self.outgoing.remove(i);
            self.events.push_back(Event::Delivered { to, seq });
        }
    }

    /// Run timers: retransmissions, failures and cache expiry.
    pub fn tick(&mut self, now: Micros) {
        if now >= self.last_prune + PRUNE_EVERY {
            self.seen.retain(|_, exp| *exp > now);
            self.delivered.retain(|_, exp| *exp > now);
            self.last_prune = now;
        }
        let mut i = 0;
        while i < self.outgoing.len() {
            let o = &self.outgoing[i];
            if o.deadline > now {
                i += 1;
                continue;
            }
            if o.attempts > self.config.max_retries {
                let o = self.outgoing.remove(i);
                if let Some(c) = self.contacts.get(&o.to) {
                    self.events.push_back(Event::Failed {
                        to: c.addr,
                        seq: o.seq,
                    });
                }
                continue;
            }
            if o.last_routed && o.routed_attempts >= 2 {
                let to = o.to;
                if let Some(c) = self.contacts.get_mut(&to) {
                    c.route = None;
                }
            }
            self.transmit_outgoing(i, now);
            i += 1;
        }
    }

    /// Next frame due for transmission, most urgent first.
    pub fn poll_transmit(&mut self, now: Micros) -> Option<Vec<u8>> {
        let best = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, q)| q.at <= now)
            .min_by_key(|(_, q)| (q.prio, q.at))
            .map(|(i, _)| i)?;
        Some(self.queue.remove(best).frame)
    }

    /// Earliest time at which [`Node::tick`] or [`Node::poll_transmit`] has work.
    pub fn next_wakeup(&self) -> Option<Micros> {
        let q = self.queue.iter().map(|q| q.at);
        let o = self.outgoing.iter().map(|o| o.deadline);
        q.chain(o).min()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

fn announce_signed_bytes(id: u32, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(8 + body.len());
    m.extend_from_slice(b"nmwA");
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn reversed(path: &[u8]) -> Vec<u8> {
    path.iter().rev().copied().collect()
}

fn max_flood_delay(air: Micros) -> Micros {
    2 * air + air / 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Lossless test medium: `links[i]` lists the nodes that hear node `i`.
    /// Every transmission is delivered 1 ms after it is sent.
    fn run(
        nodes: &mut [Node],
        links: &[Vec<usize>],
        from: Micros,
        until: Micros,
    ) -> Vec<(usize, Event)> {
        let mut events = Vec::new();
        let mut t = from;
        while t < until {
            for i in 0..nodes.len() {
                nodes[i].tick(t);
                while let Some(frame) = nodes[i].poll_transmit(t) {
                    for &j in &links[i] {
                        nodes[j].handle_frame(&frame, 0.0, t + 1_000);
                    }
                }
            }
            for (i, n) in nodes.iter_mut().enumerate() {
                while let Some(e) = n.poll_event() {
                    events.push((i, e));
                }
            }
            t += 10_000;
        }
        events
    }

    fn node(seed: u8, role: Role) -> Node {
        Node::new(
            Identity::from_seed(&[seed; 32]),
            NodeConfig::new(role, "n"),
            0,
        )
    }

    fn messages(events: &[(usize, Event)], at: usize) -> Vec<Vec<u8>> {
        events
            .iter()
            .filter_map(|(i, e)| match e {
                Event::Message { data, .. } if *i == at => Some(data.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn direct_neighbours_exchange_message() {
        let mut nodes = vec![node(1, Role::Client), node(2, Role::Client)];
        let links = vec![vec![1], vec![0]];
        nodes[0].announce(0);
        nodes[1].announce(0);
        run(&mut nodes, &links, 0, 1_000_000);
        let b = nodes[1].address();
        assert_eq!(nodes[0].route_to(&b), Some(&[][..]));

        let seq = nodes[0]
            .send(&b, crate::port::TEXT, b"hi", 1_000_000)
            .unwrap();
        let ev = run(&mut nodes, &links, 1_000_000, 5_000_000);
        assert_eq!(messages(&ev, 1), vec![b"hi".to_vec()]);
        assert!(ev.contains(&(0, Event::Delivered { to: b, seq })));
    }

    #[test]
    fn repeater_relays_and_route_is_learned() {
        // A <-> R <-> B; A and B cannot hear each other.
        let mut nodes = vec![
            node(1, Role::Client),
            node(3, Role::Repeater),
            node(2, Role::Client),
        ];
        let links = vec![vec![1], vec![0, 2], vec![1]];
        for n in nodes.iter_mut() {
            n.announce(0);
        }
        run(&mut nodes, &links, 0, 3_000_000);
        let a = nodes[0].address();
        let b = nodes[2].address();
        let r_tag = nodes[1].address().short().tag();
        assert_eq!(nodes[0].route_to(&b), Some(&[r_tag][..]));

        nodes[0]
            .send(&b, crate::port::TEXT, b"one", 3_000_000)
            .unwrap();
        nodes[2]
            .send(&a, crate::port::TEXT, b"two", 3_000_000)
            .unwrap();
        let ev = run(&mut nodes, &links, 3_000_000, 10_000_000);
        assert_eq!(messages(&ev, 2), vec![b"one".to_vec()]);
        assert_eq!(messages(&ev, 0), vec![b"two".to_vec()]);
        assert_eq!(
            ev.iter()
                .filter(|(_, e)| matches!(e, Event::Delivered { .. }))
                .count(),
            2
        );
        assert_eq!(nodes[0].pending_messages(), 0);
    }

    #[test]
    fn broken_route_falls_back_to_flood() {
        // A learns the route via R1, then R1 disappears; R2 still connects A and B.
        let mut nodes = vec![
            node(1, Role::Client),
            node(3, Role::Repeater),
            node(2, Role::Client),
            node(4, Role::Repeater),
        ];
        let links = vec![vec![1], vec![0, 2], vec![1], vec![]];
        for n in nodes.iter_mut() {
            n.announce(0);
        }
        run(&mut nodes, &links, 0, 3_000_000);
        let b = nodes[2].address();
        let r2_tag = nodes[3].address().short().tag();

        let links = vec![vec![3], vec![], vec![3], vec![0, 2]];
        nodes[0]
            .send(&b, crate::port::TEXT, b"via r2", 3_000_000)
            .unwrap();
        let ev = run(&mut nodes, &links, 3_000_000, 120_000_000);
        assert_eq!(messages(&ev, 2), vec![b"via r2".to_vec()]);
        assert!(ev
            .iter()
            .any(|(i, e)| *i == 0 && matches!(e, Event::Delivered { .. })));
        assert_eq!(nodes[0].route_to(&b), Some(&[r2_tag][..]));
    }

    #[test]
    fn unknown_contact_and_too_large() {
        let mut a = node(1, Role::Client);
        let b = node(2, Role::Client).address();
        assert_eq!(a.send(&b, 1, b"x", 0), Err(SendError::UnknownContact));
        let mut nodes = vec![a, node(2, Role::Client)];
        nodes[1].announce(0);
        run(&mut nodes, &[vec![1], vec![0]], 0, 1_000_000);
        a = nodes.remove(0);
        assert_eq!(a.send(&b, 1, &[0; 250], 0), Err(SendError::TooLarge));
        assert!(a.send(&b, 1, &[0; 200], 0).is_ok());
    }

    #[test]
    fn tampered_frame_is_ignored() {
        let mut nodes = vec![node(1, Role::Client), node(2, Role::Client)];
        nodes[0].announce(0);
        nodes[1].announce(0);
        run(&mut nodes, &[vec![1], vec![0]], 0, 1_000_000);
        let b = nodes[1].address();
        nodes[0].send(&b, 1, b"secret", 1_000_000).unwrap();
        let mut frame = nodes[0].poll_transmit(1_000_000).unwrap();
        let last = frame.len() - 1;
        frame[last] ^= 0x55;
        nodes[1].handle_frame(&frame, 0.0, 1_001_000);
        assert_eq!(nodes[1].poll_event(), None);
    }
}
