//! Radio medium model shared by the `nmw-sim` benchmark and the virtual mesh
//! in `nmw-host`.
//!
//! Nodes are placed at random in a square area. A transmission reaches every
//! node within radio range unless it collides with another overlapping
//! transmission audible at the receiver (unless it is at least
//! [`CAPTURE_DB`] stronger: the LoRa capture effect), the receiver is itself
//! transmitting, or it is dropped by the random per-link loss. Senders use
//! listen-before-talk with random backoff, as the firmware will.

use nmw_core::{LoraProfile, Node};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::VecDeque;

pub const S: u64 = 1_000_000;
/// LoRa co-channel capture: a frame survives overlap if it is this much stronger than each interferer.
pub const CAPTURE_DB: f32 = 6.0;

/// Shape of a random topology. Nodes `0..repeaters` are the repeaters.
#[derive(Clone, Copy, Debug)]
pub struct TopologyParams {
    pub nodes: usize,
    pub repeaters: usize,
    pub area_m: f64,
    pub range_m: f64,
}

pub struct Topology {
    pub pos: Vec<(f64, f64)>,
    pub range: f64,
    pub neighbours: Vec<Vec<usize>>,
}

impl Topology {
    pub fn dist(&self, a: usize, b: usize) -> f64 {
        let (x1, y1) = self.pos[a];
        let (x2, y2) = self.pos[b];
        ((x1 - x2).powi(2) + (y1 - y2).powi(2)).sqrt()
    }

    pub fn in_range(&self, a: usize, b: usize) -> bool {
        a != b && self.dist(a, b) <= self.range
    }

    /// Linear SNR model: +10 dB next to the sender, -15 dB at the edge of range.
    pub fn snr(&self, a: usize, b: usize) -> f32 {
        (10.0 - 25.0 * self.dist(a, b) / self.range) as f32
    }

    /// Place nodes until repeaters form a connected backbone and every client hears a repeater.
    pub fn generate(p: &TopologyParams, rng: &mut impl Rng) -> Topology {
        for _ in 0..10_000 {
            let pos: Vec<(f64, f64)> = (0..p.nodes)
                .map(|_| (rng.gen::<f64>() * p.area_m, rng.gen::<f64>() * p.area_m))
                .collect();
            let mut t = Topology {
                pos,
                range: p.range_m,
                neighbours: Vec::new(),
            };
            t.neighbours = (0..p.nodes)
                .map(|i| (0..p.nodes).filter(|&j| t.in_range(i, j)).collect())
                .collect();
            let reps = 0..p.repeaters;
            let clients_ok =
                (p.repeaters..p.nodes).all(|c| t.neighbours[c].iter().any(|&n| reps.contains(&n)));
            if clients_ok && t.backbone_connected(p.repeaters) {
                return t;
            }
        }
        panic!("could not generate a connected topology; increase the range or the repeaters");
    }

    /// Fewest radio hops from `a` to `b` when only repeaters relay.
    pub fn hops(&self, a: usize, b: usize, repeaters: usize) -> usize {
        let mut dist = vec![usize::MAX; self.pos.len()];
        let mut queue = VecDeque::from([a]);
        dist[a] = 0;
        while let Some(i) = queue.pop_front() {
            if i == b {
                return dist[i];
            }
            if i != a && i >= repeaters {
                continue;
            }
            for &j in &self.neighbours[i] {
                if dist[j] == usize::MAX {
                    dist[j] = dist[i] + 1;
                    queue.push_back(j);
                }
            }
        }
        usize::MAX
    }

    fn backbone_connected(&self, repeaters: usize) -> bool {
        if repeaters == 0 {
            return true;
        }
        let mut seen = vec![false; repeaters];
        let mut stack = vec![0];
        seen[0] = true;
        while let Some(i) = stack.pop() {
            for &j in &self.neighbours[i] {
                if j < repeaters && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        seen.iter().all(|&s| s)
    }
}

/// What happened to one reception.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxOutcome {
    Received,
    Collision,
    Lost,
}

/// Radio activity, recorded when logging is enabled.
#[derive(Clone, Debug)]
pub enum RadioEvent {
    Transmit {
        at: u64,
        from: usize,
        airtime: u64,
        frame: Vec<u8>,
    },
    Receive {
        at: u64,
        from: usize,
        to: usize,
        snr_db: f32,
        outcome: RxOutcome,
        frame: Vec<u8>,
    },
}

#[derive(Default, Clone, Debug)]
pub struct MediumStats {
    pub transmissions: usize,
    pub receptions: usize,
    pub collisions: usize,
    /// Airtime per node, microseconds.
    pub airtime: Vec<u64>,
}

struct Tx {
    from: usize,
    start: u64,
    end: u64,
    frame: Vec<u8>,
    done: bool,
}

#[derive(Default)]
struct Radio {
    pending: Option<Vec<u8>>,
    ready_at: u64,
    busy_until: u64,
}

/// The shared radio channel between `topology.pos.len()` nodes.
pub struct Medium {
    pub topology: Topology,
    pub profile: LoraProfile,
    pub loss: f64,
    pub stats: MediumStats,
    rng: SmallRng,
    txs: Vec<Tx>,
    radios: Vec<Radio>,
    log: Option<Vec<RadioEvent>>,
}

impl Medium {
    pub fn new(topology: Topology, profile: LoraProfile, loss: f64, seed: u64) -> Medium {
        let n = topology.pos.len();
        Medium {
            topology,
            profile,
            loss,
            stats: MediumStats {
                airtime: vec![0; n],
                ..Default::default()
            },
            rng: SmallRng::seed_from_u64(seed),
            txs: Vec::new(),
            radios: (0..n).map(|_| Radio::default()).collect(),
            log: None,
        }
    }

    /// Record [`RadioEvent`]s for [`Medium::take_log`].
    pub fn enable_log(&mut self) {
        self.log.get_or_insert_with(Vec::new);
    }

    pub fn take_log(&mut self) -> Vec<RadioEvent> {
        self.log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Hand every transmission that has ended by `t` to the nodes that hear it.
    pub fn deliver(&mut self, nodes: &mut [Node], t: u64) {
        for k in 0..self.txs.len() {
            if self.txs[k].done || self.txs[k].end > t {
                continue;
            }
            self.txs[k].done = true;
            let tx = &self.txs[k];
            for &r in &self.topology.neighbours[tx.from] {
                self.stats.receptions += 1;
                let snr = self.topology.snr(tx.from, r);
                let collided = self.txs.iter().enumerate().any(|(m, o)| {
                    m != k
                        && o.start < tx.end
                        && o.end > tx.start
                        && (o.from == r
                            || (self.topology.in_range(o.from, r)
                                && snr < self.topology.snr(o.from, r) + CAPTURE_DB))
                });
                let outcome = if collided {
                    self.stats.collisions += 1;
                    RxOutcome::Collision
                } else if self.rng.gen::<f64>() < self.loss {
                    RxOutcome::Lost
                } else {
                    nodes[r].handle_frame(&tx.frame, snr, t);
                    RxOutcome::Received
                };
                if let Some(log) = self.log.as_mut() {
                    log.push(RadioEvent::Receive {
                        at: t,
                        from: tx.from,
                        to: r,
                        snr_db: snr,
                        outcome,
                        frame: tx.frame.clone(),
                    });
                }
            }
        }
        self.txs.retain(|o| !o.done || o.end + 2 * S > t);
    }

    /// Start transmissions for nodes with frames due, after listen-before-talk.
    /// Call after ticking the nodes.
    pub fn transmit(&mut self, nodes: &mut [Node], t: u64) {
        for (i, node) in nodes.iter_mut().enumerate() {
            let radio = &mut self.radios[i];
            if radio.busy_until > t {
                continue;
            }
            if radio.pending.is_none() {
                radio.pending = node.poll_transmit(t);
                radio.ready_at = t;
            }
            let Some(frame_len) = radio.pending.as_ref().map(|f| f.len()) else {
                continue;
            };
            if radio.ready_at > t {
                continue;
            }
            let topo = &self.topology;
            let busy = self
                .txs
                .iter()
                .any(|o| o.start <= t && o.end > t && topo.in_range(o.from, i));
            let air = self.profile.airtime_us(frame_len);
            if busy {
                radio.ready_at = t + self.rng.gen_range(5_000..air.max(5_001));
                continue;
            }
            let frame = radio.pending.take().expect("checked above");
            radio.busy_until = t + air;
            self.stats.airtime[i] += air;
            self.stats.transmissions += 1;
            if let Some(log) = self.log.as_mut() {
                log.push(RadioEvent::Transmit {
                    at: t,
                    from: i,
                    airtime: air,
                    frame: frame.clone(),
                });
            }
            self.txs.push(Tx {
                from: i,
                start: t,
                end: t + air,
                frame,
                done: false,
            });
        }
    }

    /// Earliest time after `t` at which the medium or a node has work.
    pub fn next_event(&self, nodes: &[Node], t: u64) -> Option<u64> {
        let mut next = None;
        let mut consider = |c: u64| {
            if c > t && next.is_none_or(|n| c < n) {
                next = Some(c);
            }
        };
        self.txs
            .iter()
            .filter(|o| !o.done)
            .for_each(|o| consider(o.end));
        for (r, node) in self.radios.iter().zip(nodes) {
            consider(r.busy_until);
            if r.pending.is_some() {
                consider(r.ready_at);
            }
            if let Some(w) = node.next_wakeup() {
                consider(w);
            }
        }
        next
    }
}
