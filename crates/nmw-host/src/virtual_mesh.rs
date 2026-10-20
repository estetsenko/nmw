//! This node plus a simulated mesh around it, with a couple of bots to talk to.

use crate::{
    addr_to_hex, channel_id_to_hex, resolve_channel, ChannelInfo, ContactInfo, HostEvent, Mesh,
    RadioLogEntry, Status,
};
use nmw_core::{
    port, Address, Channel, Event, Identity, LoraProfile, Node, NodeConfig, Packet, PacketType,
    Role, ShortAddr, MAX_DATA, MAX_GROUP_DATA,
};
use nmw_sim::{Medium, RadioEvent, RxOutcome, Topology, TopologyParams, S};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::HashMap;

/// Every simulated node announces twice soon after start (once in each
/// window, spread out to avoid an announce storm), so the contact list fills
/// up within a couple of minutes; then every 15 minutes.
const ANNOUNCE_ROUNDS: [std::ops::Range<u64>; 2] = [0..45 * S, 60 * S..120 * S];
const REANNOUNCE: u64 = 15 * 60 * S;
/// Bots take a moment to "type" their reply.
const BOT_DELAY: std::ops::Range<u64> = 800_000..2_000_000;

pub const ECHO_BOT: &str = "Echo bot";
pub const INFO_BOT: &str = "Info bot";
/// The public channel the simulated users and Info bot chat in.
pub const PUBLIC_CHANNEL: &str = "#public";
/// Simulated users post in the public channel this often.
const CHATTER_FIRST: std::ops::Range<u64> = 40 * S..240 * S;
const CHATTER_EVERY: std::ops::Range<u64> = 4 * 60 * S..12 * 60 * S;
const CHATTER: &[&str] = &[
    "Good morning, mesh!",
    "Anyone near the river? Signal is great here.",
    "Testing my new antenna.",
    "Heading out for a hike, will check in later.",
    "Weather is clearing up in the east.",
    "Is the hill repeater back up?",
    "Coffee meetup at noon, usual place.",
    "Battery at 80%, solar works.",
];

#[derive(Clone, Debug)]
pub struct VirtualMeshConfig {
    pub nodes: usize,
    pub repeaters: usize,
    pub area_m: f64,
    pub range_m: f64,
    /// Random per-link loss on top of collisions.
    pub loss: f64,
    /// Seed for the topology, the other nodes' identities and the radio model.
    pub seed: u64,
    /// Seed of this node's identity (persist it so the address stays the same).
    pub my_seed: [u8; 32],
    pub my_name: String,
}

impl Default for VirtualMeshConfig {
    fn default() -> Self {
        VirtualMeshConfig {
            nodes: 20,
            repeaters: 6,
            area_m: 4000.0,
            range_m: 1500.0,
            loss: 0.1,
            seed: 7,
            my_seed: [1; 32],
            my_name: "Me".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bot {
    Echo,
    Info,
}

pub struct VirtualMesh {
    nodes: Vec<Node>,
    names: Vec<String>,
    bots: Vec<Option<Bot>>,
    index: HashMap<Address, usize>,
    medium: Medium,
    me: usize,
    repeaters: usize,
    t: u64,
    /// Pending announce times per node, latest first.
    announces: Vec<Vec<u64>>,
    /// (when, bot node, recipient, text)
    replies: Vec<(u64, usize, Address, String)>,
    /// (when, node, text) posts to the public channel by simulated nodes.
    group_posts: Vec<(u64, usize, String)>,
    public: Channel,
    /// Channels this node has joined.
    my_channels: Vec<ChannelInfo>,
    by_short: HashMap<ShortAddr, usize>,
    events: Vec<HostEvent>,
    rng: SmallRng,
}

impl VirtualMesh {
    pub fn new(cfg: VirtualMeshConfig) -> VirtualMesh {
        assert!(
            cfg.nodes >= cfg.repeaters + 3,
            "need room for this node and two bots"
        );
        let mut rng = SmallRng::seed_from_u64(cfg.seed);
        let params = TopologyParams {
            nodes: cfg.nodes,
            repeaters: cfg.repeaters,
            area_m: cfg.area_m,
            range_m: cfg.range_m,
        };
        let topology = Topology::generate(&params, &mut rng);
        let me = cfg.repeaters;

        let mut names = Vec::with_capacity(cfg.nodes);
        let mut bots = Vec::with_capacity(cfg.nodes);
        let mut nodes = Vec::with_capacity(cfg.nodes);
        let mut publics = Vec::with_capacity(cfg.nodes);
        for i in 0..cfg.nodes {
            let (name, bot) = if i < cfg.repeaters {
                (format!("Repeater {}", i + 1), None)
            } else if i == me {
                (cfg.my_name.clone(), None)
            } else if i == me + 1 {
                (ECHO_BOT.to_string(), Some(Bot::Echo))
            } else if i == me + 2 {
                (INFO_BOT.to_string(), Some(Bot::Info))
            } else {
                (format!("Sim user {}", i - me - 2), None)
            };
            let seed = if i == me {
                cfg.my_seed
            } else {
                let mut s = [0u8; 32];
                rng.fill(&mut s);
                s
            };
            let role = if i < cfg.repeaters {
                Role::Repeater
            } else {
                Role::Client
            };
            let mut node_cfg = NodeConfig::new(role, &name);
            node_cfg.rng_seed = rng.gen();
            let identity = Identity::from_seed(&seed);
            publics.push(*identity.public());
            nodes.push(Node::new(identity, node_cfg, 0));
            names.push(name);
            bots.push(bot);
        }
        let index = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.address(), i))
            .collect();
        let announces = (0..cfg.nodes)
            .map(|i| {
                if i == me {
                    vec![70 * S, S]
                } else {
                    ANNOUNCE_ROUNDS
                        .iter()
                        .rev()
                        .map(|r| rng.gen_range(r.clone()))
                        .collect()
                }
            })
            .collect();
        // The bots are pre-shared contacts, as if added from their QR codes;
        // everyone else is discovered from announces.
        let mut events = Vec::new();
        for bot in [me + 1, me + 2] {
            let public = publics[bot];
            nodes[me].add_contact(&public, &names[bot]);
            events.push(HostEvent::Contact {
                contact: ContactInfo {
                    addr: addr_to_hex(&public.address()),
                    name: names[bot].clone(),
                    hops: None,
                },
            });
        }
        // Simulated users and Info bot hang out in the public channel.
        let public = Channel::public(PUBLIC_CHANNEL);
        let mut group_posts = Vec::new();
        for i in me + 2..cfg.nodes {
            nodes[i].join_channel(&public);
            if bots[i].is_none() {
                group_posts.push((rng.gen_range(CHATTER_FIRST), i, String::new()));
            }
        }
        let by_short = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.address().short(), i))
            .collect();
        let mut medium = Medium::new(topology, LoraProfile::MEDIUM, cfg.loss, rng.gen());
        medium.enable_log();
        VirtualMesh {
            nodes,
            names,
            bots,
            index,
            medium,
            me,
            repeaters: cfg.repeaters,
            t: 0,
            announces,
            replies: Vec::new(),
            group_posts,
            public,
            my_channels: Vec::new(),
            by_short,
            events,
            rng,
        }
    }

    pub fn address(&self) -> Address {
        self.nodes[self.me].address()
    }

    fn display_name(&self, addr: &Address, announced: &str) -> String {
        if !announced.is_empty() {
            announced.to_string()
        } else {
            format!("Unknown {}", &addr_to_hex(addr)[..8])
        }
    }

    fn contact_info(&self, addr: Address, name: &str) -> ContactInfo {
        ContactInfo {
            addr: addr_to_hex(&addr),
            name: self.display_name(&addr, name),
            hops: self.nodes[self.me].route_to(&addr).map(|r| r.len()),
        }
    }

    fn bot_reply(&self, bot: Bot, bot_idx: usize, from: Option<usize>, text: &str) -> String {
        match bot {
            Bot::Echo => format!("echo: {text}"),
            Bot::Info => match text.trim() {
                "/help" => "Commands: /ping, /hops, /time, /nodes".into(),
                "/ping" => "pong".into(),
                "/hops" => match from {
                    Some(i) => match self.medium.topology.hops(i, bot_idx, self.repeaters) {
                        1 => "You are 1 radio hop from me.".into(),
                        n => format!("You are {n} radio hops from me."),
                    },
                    None => "I don't know where you are.".into(),
                },
                "/time" => {
                    let s = self.t / S;
                    format!(
                        "Mesh time is {:02}:{:02}:{:02}.",
                        s / 3600,
                        s / 60 % 60,
                        s % 60
                    )
                }
                "/nodes" => format!(
                    "This simulated mesh has {} nodes, {} of them repeaters.",
                    self.nodes.len(),
                    self.repeaters
                ),
                _ => "I'm a bot. Send /help.".into(),
            },
        }
    }

    fn handle_events(&mut self, t: u64) {
        for i in 0..self.nodes.len() {
            self.nodes[i].tick(t);
            while let Some(ev) = self.nodes[i].poll_event() {
                if i == self.me {
                    self.my_event(ev, t);
                } else if let (
                    Some(bot),
                    Event::Message {
                        from, port, data, ..
                    },
                ) = (self.bots[i], &ev)
                {
                    if *port == port::TEXT {
                        let text = String::from_utf8_lossy(data);
                        let reply = self.bot_reply(bot, i, self.index.get(from).copied(), &text);
                        let at = t + self.rng.gen_range(BOT_DELAY);
                        self.replies.push((at, i, *from, reply));
                    }
                } else if let (
                    Some(Bot::Info),
                    Event::GroupMessage {
                        channel,
                        from,
                        name,
                        port: port::TEXT,
                        data,
                        ..
                    },
                ) = (self.bots[i], &ev)
                {
                    // In the channel Info bot only answers commands.
                    let text = String::from_utf8_lossy(data);
                    if *channel == self.public.id() && text.trim_start().starts_with('/') {
                        let from_idx = self.by_short.get(from).copied();
                        let reply = self.bot_reply(Bot::Info, i, from_idx, &text);
                        let at = t + self.rng.gen_range(BOT_DELAY);
                        let who = if name.is_empty() {
                            crate::hex(&from.0)
                        } else {
                            name.clone()
                        };
                        self.group_posts.push((at, i, format!("@{who} {reply}")));
                    }
                }
            }
        }
    }

    fn my_event(&mut self, ev: Event, t: u64) {
        let e = match ev {
            Event::Contact { addr, name, .. } => HostEvent::Contact {
                contact: self.contact_info(addr, &name),
            },
            Event::Message {
                from,
                port,
                data,
                seq,
            } => HostEvent::Message {
                from: addr_to_hex(&from),
                text: if port == port::TEXT {
                    String::from_utf8_lossy(&data).into_owned()
                } else {
                    format!("[port {port}, {} bytes]", data.len())
                },
                seq,
                at_ms: t / 1000,
            },
            Event::Delivered { to, seq } => HostEvent::Delivered {
                to: addr_to_hex(&to),
                seq,
            },
            Event::Failed { to, seq } => HostEvent::Failed {
                to: addr_to_hex(&to),
                seq,
            },
            Event::GroupMessage {
                channel,
                from,
                from_addr,
                name,
                port,
                data,
            } => HostEvent::GroupMessage {
                channel: channel_id_to_hex(&channel),
                from: crate::hex(&from.0),
                from_addr: from_addr.map(|a| addr_to_hex(&a)),
                name,
                text: if port == port::TEXT {
                    String::from_utf8_lossy(&data).into_owned()
                } else {
                    format!("[port {port}, {} bytes]", data.len())
                },
                at_ms: t / 1000,
            },
        };
        self.events.push(e);
    }

    fn collect_radio_log(&mut self) {
        for ev in self.medium.take_log() {
            let entry = match ev {
                RadioEvent::Transmit {
                    at,
                    from,
                    airtime,
                    frame,
                } if from == self.me => log_entry(
                    at,
                    "tx",
                    &self.names[from],
                    &frame,
                    Some(airtime as f64 / 1000.0),
                    None,
                    None,
                ),
                RadioEvent::Receive {
                    at,
                    from,
                    to,
                    snr_db,
                    outcome,
                    frame,
                } if to == self.me => log_entry(
                    at,
                    "rx",
                    &self.names[from],
                    &frame,
                    None,
                    Some(snr_db),
                    Some(match outcome {
                        RxOutcome::Received => "ok",
                        RxOutcome::Collision => "collision",
                        RxOutcome::Lost => "lost",
                    }),
                ),
                _ => continue,
            };
            self.events.push(HostEvent::Radio(entry));
        }
    }
}

fn log_entry(
    at: u64,
    dir: &'static str,
    peer: &str,
    frame: &[u8],
    airtime_ms: Option<f64>,
    snr_db: Option<f32>,
    outcome: Option<&'static str>,
) -> RadioLogEntry {
    let packet = Packet::decode(frame).ok();
    RadioLogEntry {
        at_ms: at / 1000,
        dir,
        peer: peer.to_string(),
        kind: packet
            .as_ref()
            .map_or("INVALID", |p| match p.ptype {
                PacketType::Data => "DATA",
                PacketType::DataKey => "DATA+KEY",
                PacketType::Ack => "ACK",
                PacketType::Announce => "ANNOUNCE",
                PacketType::Group => "GROUP",
                PacketType::Ctrl => "CTRL",
            })
            .to_string(),
        len: frame.len(),
        path_len: packet.as_ref().map_or(0, |p| p.path.len()),
        routed: packet.as_ref().is_some_and(|p| p.routed),
        airtime_ms,
        snr_db,
        outcome,
    }
}

impl Mesh for VirtualMesh {
    fn advance_to(&mut self, target: u64) {
        loop {
            let t = self.t;
            self.medium.deliver(&mut self.nodes, t);
            for i in 0..self.nodes.len() {
                if self.announces[i].last().is_some_and(|&a| a <= t) {
                    self.announces[i].pop();
                    self.nodes[i].announce(t);
                    if self.announces[i].is_empty() {
                        let next = t + REANNOUNCE + self.rng.gen_range(0..60 * S);
                        self.announces[i].push(next);
                    }
                }
            }
            let (due, later): (Vec<_>, Vec<_>) =
                self.replies.drain(..).partition(|(at, ..)| *at <= t);
            self.replies = later;
            for (_, bot, to, text) in due {
                let _ = self.nodes[bot].send(&to, port::TEXT, text.as_bytes(), t);
            }
            let (due, later): (Vec<_>, Vec<_>) =
                self.group_posts.drain(..).partition(|(at, ..)| *at <= t);
            self.group_posts = later;
            for (_, i, text) in due {
                // An empty text is a simulated user's chatter timer.
                let text = if text.is_empty() {
                    let next = t + self.rng.gen_range(CHATTER_EVERY);
                    self.group_posts.push((next, i, String::new()));
                    CHATTER[self.rng.gen_range(0..CHATTER.len())].to_string()
                } else {
                    text
                };
                let _ = self.nodes[i].send_group(&self.public, port::TEXT, text.as_bytes(), t);
            }
            self.handle_events(t);
            self.medium.transmit(&mut self.nodes, t);
            self.collect_radio_log();

            let mut next = self.medium.next_event(&self.nodes, t).unwrap_or(u64::MAX);
            for &a in self.announces.iter().filter_map(|a| a.last()) {
                next = next.min(a);
            }
            for (at, ..) in &self.replies {
                next = next.min(*at);
            }
            for (at, ..) in &self.group_posts {
                next = next.min(*at);
            }
            if next > target {
                self.t = self.t.max(target);
                return;
            }
            self.t = next;
        }
    }

    fn now(&self) -> u64 {
        self.t
    }

    fn send_text(&mut self, to: &Address, text: &str) -> Result<u16, String> {
        if text.len() > MAX_DATA {
            return Err(format!(
                "message is {} bytes; the limit is {MAX_DATA}",
                text.len()
            ));
        }
        if *to == self.address() {
            return Err("that is this node's own address".into());
        }
        self.nodes[self.me]
            .send(to, port::TEXT, text.as_bytes(), self.t)
            .map_err(|e| format!("{e:?}"))
    }

    fn announce(&mut self) {
        self.nodes[self.me].announce(self.t);
    }

    fn contacts(&self) -> Vec<ContactInfo> {
        let mut list: Vec<ContactInfo> = self.nodes[self.me]
            .contacts()
            .map(|(addr, name)| self.contact_info(addr, name))
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    fn status(&self) -> Status {
        let airtime = self.medium.stats.airtime[self.me];
        Status {
            addr: addr_to_hex(&self.address()),
            name: self.names[self.me].clone(),
            backend: "virtual",
            time_ms: self.t / 1000,
            airtime_pct: if self.t == 0 {
                0.0
            } else {
                airtime as f64 / self.t as f64 * 100.0
            },
            neighbours: self.medium.topology.neighbours[self.me].len(),
            nodes: self.nodes.len(),
        }
    }

    fn drain_events(&mut self) -> Vec<HostEvent> {
        std::mem::take(&mut self.events)
    }

    fn join_channel(&mut self, name: &str, key: Option<&str>) -> Result<ChannelInfo, String> {
        let info = resolve_channel(name, key)?;
        if let Some(existing) = self.my_channels.iter().find(|c| c.key == info.key) {
            return Ok(existing.clone());
        }
        let channel = info.channel().expect("resolve_channel makes a valid key");
        self.nodes[self.me].join_channel(&channel);
        self.my_channels.push(info.clone());
        Ok(info)
    }

    fn leave_channel(&mut self, id: &str) {
        let (gone, kept) = std::mem::take(&mut self.my_channels)
            .into_iter()
            .partition(|c| c.id == id);
        self.my_channels = kept;
        for c in gone {
            if let Some(ch) = c.channel() {
                self.nodes[self.me].leave_channel(&ch);
            }
        }
    }

    fn channels(&self) -> Vec<ChannelInfo> {
        self.my_channels.clone()
    }

    fn send_group(&mut self, id: &str, text: &str) -> Result<(), String> {
        if text.len() > MAX_GROUP_DATA {
            return Err(format!(
                "message is {} bytes; the limit is {MAX_GROUP_DATA}",
                text.len()
            ));
        }
        let channel = self
            .my_channels
            .iter()
            .find(|c| c.id == id)
            .and_then(ChannelInfo::channel)
            .ok_or("not a joined channel")?;
        self.nodes[self.me]
            .send_group(&channel, port::TEXT, text.as_bytes(), self.t)
            .map_err(|e| format!("{e:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_addr;

    fn ready_mesh() -> VirtualMesh {
        let mut m = VirtualMesh::new(VirtualMeshConfig::default());
        m.advance_to(150 * S);
        m
    }

    fn contact(m: &VirtualMesh, name: &str) -> Address {
        let c = m
            .contacts()
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} not in contacts"));
        parse_addr(&c.addr).unwrap()
    }

    fn texts(events: &[HostEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                HostEvent::Message { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn bots_are_reachable() {
        let m = VirtualMesh::new(VirtualMeshConfig::default());
        for bot in [m.me + 1, m.me + 2] {
            let hops = m.medium.topology.hops(m.me, bot, m.repeaters);
            // Within the default hop limit of 6 repeaters.
            assert!(hops <= 7, "bot {bot} is {hops} hops away");
        }
    }

    #[test]
    fn contacts_fill_up_from_announces() {
        let m = ready_mesh();
        let names: Vec<String> = m.contacts().into_iter().map(|c| c.name).collect();
        assert!(names.contains(&ECHO_BOT.to_string()), "{names:?}");
        assert!(names.contains(&INFO_BOT.to_string()), "{names:?}");
        assert!(names.len() >= 10, "{names:?}");
    }

    #[test]
    fn echo_bot_answers() {
        let mut m = ready_mesh();
        let echo = contact(&m, ECHO_BOT);
        m.drain_events();
        let seq = m.send_text(&echo, "hello mesh").unwrap();
        m.advance_to(m.now() + 60 * S);
        let ev = m.drain_events();
        assert!(ev.contains(&HostEvent::Delivered {
            to: addr_to_hex(&echo),
            seq
        }));
        assert_eq!(texts(&ev), vec!["echo: hello mesh".to_string()]);
    }

    #[test]
    fn info_bot_reports_hops() {
        let mut m = ready_mesh();
        let info = contact(&m, INFO_BOT);
        m.drain_events();
        m.send_text(&info, "/hops").unwrap();
        m.advance_to(m.now() + 60 * S);
        let replies = texts(&m.drain_events());
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert!(replies[0].starts_with("You are "), "{replies:?}");
    }

    #[test]
    fn radio_log_shows_own_traffic() {
        let mut m = VirtualMesh::new(VirtualMeshConfig::default());
        m.advance_to(30 * S);
        let log: Vec<RadioLogEntry> = m
            .drain_events()
            .into_iter()
            .filter_map(|e| match e {
                HostEvent::Radio(r) => Some(r),
                _ => None,
            })
            .collect();
        assert!(log.iter().any(|r| r.dir == "tx" && r.kind == "ANNOUNCE"));
        assert!(log.iter().any(|r| r.dir == "rx" && r.outcome == Some("ok")));
        assert!(m.status().airtime_pct > 0.0);
    }

    fn group_texts(events: &[HostEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                HostEvent::GroupMessage { name, text, .. } => Some((name.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn public_channel_has_chatter_and_info_bot_answers_there() {
        let mut m = ready_mesh();
        let public = m.join_channel(PUBLIC_CHANNEL, None).unwrap();
        assert_eq!(m.channels(), vec![public.clone()]);
        m.drain_events();
        m.send_group(&public.id, "/ping").unwrap();
        m.advance_to(m.now() + 20 * 60 * S);
        let got = group_texts(&m.drain_events());
        assert!(
            got.iter()
                .any(|(n, t)| n == INFO_BOT && t.ends_with("pong")),
            "{got:?}"
        );
        assert!(
            got.iter().any(|(n, _)| n.starts_with("Sim user")),
            "{got:?}"
        );
        // Nearly every post carries a verified sender (names come from
        // announces); one whose sender's announce was lost arrives unverified.
        let verified = got.iter().filter(|(n, _)| !n.is_empty()).count();
        assert!(verified * 10 >= got.len() * 9, "{got:?}");
    }

    #[test]
    fn private_channel_is_quiet_and_leaving_stops_messages() {
        let mut m = ready_mesh();
        let team = m.join_channel("Team", None).unwrap();
        assert!(!team.public);
        let public = m.join_channel("#PUBLIC", None).unwrap();
        m.leave_channel(&public.id);
        assert_eq!(m.channels(), vec![team.clone()]);
        assert!(m.send_group(&public.id, "hi").is_err());
        m.send_group(&team.id, "only me here").unwrap();
        m.drain_events();
        m.advance_to(m.now() + 20 * 60 * S);
        assert_eq!(group_texts(&m.drain_events()), vec![]);
        assert!(m
            .send_group(&team.id, &"x".repeat(MAX_GROUP_DATA + 1))
            .is_err());
    }

    #[test]
    fn rejects_bad_sends() {
        let mut m = VirtualMesh::new(VirtualMeshConfig::default());
        let me = m.address();
        assert!(m.send_text(&me, "hi").is_err());
        let echo = Address([7; 16]);
        assert!(m.send_text(&echo, &"x".repeat(MAX_DATA + 1)).is_err());
    }

    #[test]
    fn runtime_drives_mesh_in_background() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rt = crate::Runtime::start(
            Box::new(VirtualMesh::new(VirtualMeshConfig::default())),
            200.0,
            move |e| {
                let _ = tx.send(e);
            },
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut contacts = 0;
        while contacts < 5 && std::time::Instant::now() < deadline {
            if let Ok(HostEvent::Contact { .. }) =
                rx.recv_timeout(std::time::Duration::from_millis(100))
            {
                contacts += 1;
            }
        }
        assert!(contacts >= 5);
        assert!(rt.with(|m| m.now()) > 0);
    }
}
