//! Deterministic multi-node benchmark for the NMW mesh protocol: random
//! topologies, a message workload, and delivery statistics. The radio model
//! is in the `nmw_sim` library.

use nmw_core::{port, Address, Channel, Event, Identity, LoraProfile, Node, NodeConfig, Role};
use nmw_sim::{Medium, Topology, TopologyParams, S};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::{BTreeMap, HashMap};
use std::process::ExitCode;

/// Each node announces twice while the mesh warms up (once in each window,
/// as nodes boot at different times), then every few hours.
const ANNOUNCE_ROUNDS: [(u64, u64); 2] = [(0, 300 * S), (600 * S, 900 * S)];
const REANNOUNCE: u64 = 3 * 3600 * S;
/// Message traffic starts once both announce rounds are over.
const TRAFFIC_START: u64 = 960 * S;
const DUTY_CYCLE_LIMIT: f64 = 0.10;

struct Args {
    nodes: usize,
    repeaters: usize,
    loss: f64,
    messages: usize,
    interval_s: f64,
    area_m: f64,
    range_m: f64,
    seed: u64,
    seeds: u64,
    shared_contacts: bool,
    group: bool,
    min_delivery: Option<f64>,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut a = Args {
            nodes: 50,
            repeaters: 15,
            loss: 0.2,
            messages: 300,
            interval_s: 10.0,
            area_m: 6000.0,
            range_m: 2000.0,
            seed: 1,
            seeds: 5,
            shared_contacts: false,
            group: false,
            min_delivery: None,
        };
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let mut it = argv.iter();
        while let Some(flag) = it.next() {
            if flag == "--help" || flag == "-h" {
                return Err(String::new());
            }
            if flag == "--shared-contacts" {
                a.shared_contacts = true;
                continue;
            }
            if flag == "--group" {
                a.group = true;
                continue;
            }
            let value = it.next().ok_or(format!("missing value for {flag}"))?;
            fn num<T: std::str::FromStr>(flag: &str, v: &str) -> Result<T, String> {
                v.parse().map_err(|_| format!("bad value for {flag}: {v}"))
            }
            match flag.as_str() {
                "--nodes" => a.nodes = num(flag, value)?,
                "--repeaters" => a.repeaters = num(flag, value)?,
                "--loss" => a.loss = num(flag, value)?,
                "--messages" => a.messages = num(flag, value)?,
                "--interval" => a.interval_s = num(flag, value)?,
                "--area" => a.area_m = num(flag, value)?,
                "--range" => a.range_m = num(flag, value)?,
                "--seed" => a.seed = num(flag, value)?,
                "--seeds" => a.seeds = num(flag, value)?,
                "--min-delivery" => a.min_delivery = Some(num(flag, value)?),
                _ => return Err(format!("unknown flag {flag}")),
            }
        }
        if a.repeaters >= a.nodes {
            return Err("--repeaters must be less than --nodes".into());
        }
        Ok(a)
    }
}

const USAGE: &str = "usage: nmw-sim [--nodes 50] [--repeaters 15] [--loss 0.2] [--messages 300]
               [--interval 10] [--area 6000] [--range 2000] [--seed 1] [--seeds 5]
               [--shared-contacts] [--group] [--min-delivery 0.95]

Runs --seeds independent topologies (seeds seed..seed+seeds) and reports totals.
--shared-contacts preloads every client's contacts (as if exchanged by QR code),
so delivery does not depend on hearing announces.
--group makes every client join one channel and post the messages there;
delivery then counts (message, other client) pairs.
--min-delivery gates on the share of all messages delivered. Messages to a
destination the sender has not heard an announce from wait for a contact request.";

struct Sent {
    at: u64,
    hops: usize,
    /// The sender knew the destination when the message was sent.
    known: bool,
    delivered_at: Option<u64>,
    acked: bool,
}

#[derive(Default)]
struct Stats {
    sent: usize,
    unknown: usize,
    unknown_delivered: usize,
    delivered: usize,
    acked: usize,
    duplicates: usize,
    known_pairs: usize,
    pairs: usize,
    transmissions: usize,
    receptions: usize,
    collisions: usize,
    max_duty: f64,
    sim_time: u64,
    latencies: Vec<f64>,
    /// hops -> (sent, delivered)
    by_hops: BTreeMap<usize, (usize, usize)>,
}

impl Stats {
    fn merge(&mut self, o: Stats) {
        self.sent += o.sent;
        self.unknown += o.unknown;
        self.unknown_delivered += o.unknown_delivered;
        self.delivered += o.delivered;
        self.acked += o.acked;
        self.duplicates += o.duplicates;
        self.known_pairs += o.known_pairs;
        self.pairs += o.pairs;
        self.transmissions += o.transmissions;
        self.receptions += o.receptions;
        self.collisions += o.collisions;
        self.max_duty = self.max_duty.max(o.max_duty);
        self.sim_time = self.sim_time.max(o.sim_time);
        self.latencies.extend(o.latencies);
        for (h, (n, d)) in o.by_hops {
            let e = self.by_hops.entry(h).or_default();
            e.0 += n;
            e.1 += d;
        }
    }

    fn delivery(&self) -> f64 {
        self.delivered as f64 / self.sent.max(1) as f64
    }
}

fn simulate(args: &Args, seed: u64) -> Stats {
    let mut rng = SmallRng::seed_from_u64(seed);
    let params = TopologyParams {
        nodes: args.nodes,
        repeaters: args.repeaters,
        area_m: args.area_m,
        range_m: args.range_m,
    };
    let topo = Topology::generate(&params, &mut rng);

    let mut identities = Vec::with_capacity(args.nodes);
    let mut nodes: Vec<Node> = (0..args.nodes)
        .map(|i| {
            let role = if i < args.repeaters {
                Role::Repeater
            } else {
                Role::Client
            };
            let mut key_seed = [0u8; 32];
            rng.fill(&mut key_seed);
            let mut cfg = NodeConfig::new(role, &format!("node{i}"));
            cfg.rng_seed = rng.gen();
            let identity = Identity::from_seed(&key_seed);
            identities.push(*identity.public());
            Node::new(identity, cfg, 0)
        })
        .collect();
    if args.shared_contacts {
        for c in args.repeaters..args.nodes {
            for (d, public) in identities.iter().enumerate().skip(args.repeaters) {
                if c != d {
                    nodes[c].add_contact(public, "");
                }
            }
        }
    }
    let channel = Channel::public("#sim");
    if args.group {
        for node in &mut nodes[args.repeaters..] {
            node.join_channel(&channel);
        }
    }
    let index: HashMap<Address, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.address(), i))
        .collect();
    let mut announces: Vec<Vec<u64>> = (0..args.nodes)
        .map(|_| {
            ANNOUNCE_ROUNDS
                .iter()
                .rev()
                .map(|&(a, b)| rng.gen_range(a..b))
                .collect()
        })
        .collect();
    let mut medium = Medium::new(topo, LoraProfile::MEDIUM, args.loss, rng.gen());

    let interval = (args.interval_s * S as f64) as u64;
    let end = TRAFFIC_START + interval * args.messages as u64 + 180 * S;
    let mut next_msg = 0usize;
    let mut sent: HashMap<(usize, usize, u16), Sent> = HashMap::new();
    // Group mode: (message number, receiver) -> Sent, keyed by the text "msg N".
    let mut group_sent: HashMap<(usize, usize), Sent> = HashMap::new();
    let mut st = Stats::default();

    let mut t = 0u64;
    while t <= end {
        medium.deliver(&mut nodes, t);

        // Application workload.
        for i in 0..args.nodes {
            if announces[i].last().is_some_and(|&a| a <= t) {
                announces[i].pop();
                nodes[i].announce(t);
                if announces[i].is_empty() {
                    announces[i].push(t + REANNOUNCE + rng.gen_range(0..600 * S));
                }
            }
        }
        while next_msg < args.messages && TRAFFIC_START + interval * next_msg as u64 <= t {
            next_msg += 1;
            let src = rng.gen_range(args.repeaters..args.nodes);
            if args.group {
                let text = format!("msg {next_msg}");
                nodes[src]
                    .send_group(&channel, port::TEXT, text.as_bytes(), t)
                    .expect("short text fits in one packet");
                for dst in (args.repeaters..args.nodes).filter(|&d| d != src) {
                    group_sent.insert(
                        (next_msg, dst),
                        Sent {
                            at: t,
                            hops: medium.topology.hops(src, dst, args.repeaters),
                            known: true,
                            delivered_at: None,
                            acked: false,
                        },
                    );
                }
                continue;
            }
            let mut dst = rng.gen_range(args.repeaters..args.nodes);
            while dst == src {
                dst = rng.gen_range(args.repeaters..args.nodes);
            }
            let to = nodes[dst].address();
            let text = format!("msg {next_msg}");
            let known = nodes[src].knows(&to);
            let seq = nodes[src]
                .send(&to, port::TEXT, text.as_bytes(), t)
                .expect("short text fits in one packet");
            sent.insert(
                (src, dst, seq),
                Sent {
                    at: t,
                    hops: medium.topology.hops(src, dst, args.repeaters),
                    known,
                    delivered_at: None,
                    acked: false,
                },
            );
        }

        // Node timers and events.
        for (i, node) in nodes.iter_mut().enumerate() {
            node.tick(t);
            while let Some(ev) = node.poll_event() {
                match ev {
                    Event::Message { from, seq, .. } => {
                        if let Some(s) = sent.get_mut(&(index[&from], i, seq)) {
                            if s.delivered_at.is_some() {
                                st.duplicates += 1;
                            } else {
                                s.delivered_at = Some(t);
                            }
                        }
                    }
                    Event::Delivered { to, seq } => {
                        if let Some(s) = sent.get_mut(&(i, index[&to], seq)) {
                            s.acked = true;
                        }
                    }
                    Event::GroupMessage { data, .. } => {
                        let n = std::str::from_utf8(&data)
                            .ok()
                            .and_then(|t| t.strip_prefix("msg "))
                            .and_then(|n| n.parse::<usize>().ok());
                        if let Some(s) = n.and_then(|n| group_sent.get_mut(&(n, i))) {
                            if s.delivered_at.is_some() {
                                st.duplicates += 1;
                            } else {
                                s.delivered_at = Some(t);
                            }
                        }
                    }
                    Event::Failed { .. } | Event::Contact { .. } => {}
                }
            }
        }
        medium.transmit(&mut nodes, t);

        // Advance to the next thing that can happen.
        let mut next = medium.next_event(&nodes, t).unwrap_or(u64::MAX);
        for &a in announces.iter().filter_map(|a| a.last()) {
            if a > t {
                next = next.min(a);
            }
        }
        if next_msg < args.messages {
            next = next.min(TRAFFIC_START + interval * next_msg as u64);
        }
        if next == u64::MAX {
            break;
        }
        t = next;
    }

    st.sim_time = end;
    st.sent = sent.len() + group_sent.len();
    for s in sent.values().chain(group_sent.values()) {
        let e = st.by_hops.entry(s.hops).or_default();
        e.0 += 1;
        st.unknown += !s.known as usize;
        if let Some(d) = s.delivered_at {
            st.delivered += 1;
            st.unknown_delivered += !s.known as usize;
            e.1 += 1;
            st.latencies.push((d - s.at) as f64 / S as f64);
        }
        st.acked += s.acked as usize;
    }
    st.transmissions = medium.stats.transmissions;
    st.receptions = medium.stats.receptions;
    st.collisions = medium.stats.collisions;
    st.max_duty = medium
        .stats
        .airtime
        .iter()
        .map(|&a| a as f64 / end as f64)
        .fold(0.0, f64::max);
    let clients = args.repeaters..args.nodes;
    st.pairs = clients.len() * (clients.len() - 1);
    st.known_pairs = clients
        .clone()
        .map(|c| {
            clients
                .clone()
                .filter(|&d| d != c && nodes[c].knows(&nodes[d].address()))
                .count()
        })
        .sum();
    st
}

fn pct(a: usize, b: usize) -> f64 {
    a as f64 / b.max(1) as f64 * 100.0
}

fn main() -> ExitCode {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let profile = LoraProfile::MEDIUM;
    println!(
        "nmw-sim nodes={} repeaters={} loss={:.0}% area={}m range={}m profile=SF{}/BW{}k interval={}s contacts={}",
        args.nodes,
        args.repeaters,
        args.loss * 100.0,
        args.area_m,
        args.range_m,
        profile.spreading_factor,
        profile.bandwidth_hz / 1000,
        args.interval_s,
        if args.shared_contacts { "shared" } else { "announce" }
    );
    if args.group {
        println!(
            "  group mode: every client is in one channel; acknowledged and discovery do not apply"
        );
    }

    let mut total = Stats::default();
    for seed in args.seed..args.seed + args.seeds {
        let st = simulate(&args, seed);
        println!(
            "  seed {seed:<4} delivered {:>5.1}% of {} sent, contacts known {:>5.1}%",
            st.delivery() * 100.0,
            st.sent,
            pct(st.known_pairs, st.pairs)
        );
        total.merge(st);
    }

    let lat = &mut total.latencies;
    lat.sort_by(|a, b| a.partial_cmp(b).expect("latencies are finite"));
    let q = |p: f64| {
        lat.get(((lat.len() as f64 * p) as usize).min(lat.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    };
    let (p50, p95, max) = (q(0.5), q(0.95), q(1.0));
    let hops: Vec<String> = total
        .by_hops
        .iter()
        .map(|(h, (n, d))| format!("{h}:{:.0}%", pct(*d, *n)))
        .collect();

    println!("total over {} seeds", args.seeds);
    println!(
        "  delivered           {} of {} sent ({:.1}%)",
        total.delivered,
        total.sent,
        total.delivery() * 100.0
    );
    println!(
        "  acknowledged        {:.1}%, duplicate deliveries {}",
        pct(total.acked, total.sent),
        total.duplicates
    );
    println!(
        "  discovery           {:.1}% of client pairs known at the end",
        pct(total.known_pairs, total.pairs)
    );
    println!(
        "  unknown at send     {} messages, {:.1}% delivered after a contact request",
        total.unknown,
        pct(total.unknown_delivered, total.unknown)
    );
    println!("  delivered by hops   {}", hops.join("  "));
    println!("  latency             p50 {p50:.2}s  p95 {p95:.2}s  max {max:.2}s");
    println!(
        "  collisions          {:.1}% of receptions",
        pct(total.collisions, total.receptions)
    );
    println!(
        "  busiest node        {:.2}% airtime over {:.0} min (EU 869.525 MHz limit {:.0}%)",
        total.max_duty * 100.0,
        total.sim_time as f64 / (60 * S) as f64,
        DUTY_CYCLE_LIMIT * 100.0
    );

    if let Some(min) = args.min_delivery {
        let ratio = total.delivery();
        if ratio < min {
            eprintln!(
                "FAIL: delivery {:.1}% below required {:.1}%",
                ratio * 100.0,
                min * 100.0
            );
            return ExitCode::FAILURE;
        }
        println!(
            "PASS: delivery {:.1}% >= {:.1}%",
            ratio * 100.0,
            min * 100.0
        );
    }
    ExitCode::SUCCESS
}
