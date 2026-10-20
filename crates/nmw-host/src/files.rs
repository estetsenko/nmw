//! Sends files to a contact a few chunks at a time and collects incoming
//! ones (wire format in [`nmw_core::file`]).

use nmw_core::file::{self, Offer, Received, Receiver};
use nmw_core::node::Micros;
use nmw_core::{port, Address, Event, Node, SendError, MAX_DATA};
use std::collections::{HashMap, VecDeque};

pub use nmw_core::file::MAX_FILE;

/// Chunks in flight per transfer. One at a time is fastest in the
/// simulator: LoRa is half duplex on one channel, so a second chunk mostly
/// collides with the ACK for the first (10 KB over one hop at 10% loss:
/// ~100 s with one, ~140 s with three).
const WINDOW: usize = 1;
/// Times a chunk (or the offer) is sent again after the node gave up on it.
const RETRIES: u8 = 2;
const ROOM: usize = file::chunk_room(MAX_DATA);

/// What happened to a transfer, in either direction. `id` is the sender's
/// transfer id, so (peer, outgoing, id) names one transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEvent {
    Offered {
        from: Address,
        id: u16,
        name: String,
        mime: String,
        size: u32,
    },
    Progress {
        peer: Address,
        id: u16,
        outgoing: bool,
        done: u16,
        count: u16,
    },
    Received {
        from: Address,
        id: u16,
        name: String,
        mime: String,
        data: Vec<u8>,
    },
    Sent {
        to: Address,
        id: u16,
    },
    Failed {
        peer: Address,
        id: u16,
        outgoing: bool,
    },
}

struct Sending {
    to: Address,
    offer: Offer,
    data: Vec<u8>,
    /// The offer was acknowledged, so chunks may go.
    accepted: bool,
    next: u16,
    in_flight: usize,
    acked: u16,
    retries: HashMap<Option<u16>, u8>,
    /// Chunks to send again before new ones.
    resend: VecDeque<u16>,
}

#[derive(Clone, Copy)]
enum Part {
    Offer,
    Chunk(u16),
    Cancel,
}

#[derive(Default)]
pub struct FileLayer {
    next_id: u16,
    sending: HashMap<u16, Sending>,
    /// Node seq -> (transfer id, what it carried).
    seqs: HashMap<u16, (u16, Part)>,
    incoming: Receiver<Address>,
    events: Vec<FileEvent>,
}

impl FileLayer {
    /// Start sending `data` to `to`. Returns the transfer id used in later
    /// [`FileEvent`]s.
    pub fn send_file(
        &mut self,
        node: &mut Node,
        to: &Address,
        name: &str,
        mime: &str,
        data: Vec<u8>,
        now: Micros,
    ) -> Result<u16, SendError> {
        let id = self.next_id;
        let offer = Offer::new(id, name, mime, &data, ROOM).ok_or(SendError::TooLarge)?;
        let seq = node.send(to, port::RESOURCE, &offer.encode(), now)?;
        self.next_id = self.next_id.wrapping_add(1);
        self.seqs.insert(seq, (id, Part::Offer));
        self.sending.insert(
            id,
            Sending {
                to: *to,
                offer,
                data,
                accepted: false,
                next: 0,
                in_flight: 0,
                acked: 0,
                retries: HashMap::new(),
                resend: VecDeque::new(),
            },
        );
        Ok(id)
    }

    /// Take the node events that belong to file transfers; pass the rest on.
    pub fn filter(&mut self, node: &mut Node, ev: Event, now: Micros) -> Option<Event> {
        match ev {
            Event::Message {
                from,
                port: port::RESOURCE,
                data,
                ..
            } => {
                self.receive(from, &data, now);
                None
            }
            Event::Delivered { to, seq } => match self.seqs.remove(&seq) {
                None => Some(Event::Delivered { to, seq }),
                Some((id, part)) => {
                    self.delivered(node, id, part, now);
                    None
                }
            },
            Event::Failed { to, seq } => match self.seqs.remove(&seq) {
                None => Some(Event::Failed { to, seq }),
                Some((id, part)) => {
                    self.failed(node, id, part, now);
                    None
                }
            },
            other => Some(other),
        }
    }

    fn receive(&mut self, from: Address, data: &[u8], now: Micros) {
        let ev = match self.incoming.push(from, data, now) {
            Received::Offered(o) => FileEvent::Offered {
                from,
                id: o.id,
                name: o.name,
                mime: o.mime,
                size: o.size,
            },
            Received::Progress {
                id,
                received,
                count,
            } => FileEvent::Progress {
                peer: from,
                id,
                outgoing: false,
                done: received,
                count,
            },
            Received::Complete { offer, data } => FileEvent::Received {
                from,
                id: offer.id,
                name: offer.name,
                mime: offer.mime,
                data,
            },
            Received::Failed { id } => FileEvent::Failed {
                peer: from,
                id,
                outgoing: false,
            },
            Received::Nothing => return,
        };
        self.events.push(ev);
    }

    fn delivered(&mut self, node: &mut Node, id: u16, part: Part, now: Micros) {
        let Some(s) = self.sending.get_mut(&id) else {
            return;
        };
        match part {
            Part::Offer => s.accepted = true,
            Part::Chunk(_) => {
                s.in_flight -= 1;
                s.acked += 1;
                self.events.push(FileEvent::Progress {
                    peer: s.to,
                    id,
                    outgoing: true,
                    done: s.acked,
                    count: s.offer.count,
                });
                if s.acked == s.offer.count {
                    let to = s.to;
                    self.sending.remove(&id);
                    self.events.push(FileEvent::Sent { to, id });
                    return;
                }
            }
            Part::Cancel => return,
        }
        self.pump(node, id, now);
    }

    fn failed(&mut self, node: &mut Node, id: u16, part: Part, now: Micros) {
        let Some(s) = self.sending.get_mut(&id) else {
            return;
        };
        let index = match part {
            Part::Offer => None,
            Part::Chunk(i) => {
                s.in_flight -= 1;
                Some(i)
            }
            Part::Cancel => return,
        };
        let tries = s.retries.entry(index).or_insert(0);
        *tries += 1;
        if *tries > RETRIES {
            let to = s.to;
            self.sending.remove(&id);
            // A failed chunk means the receiver holds a partial transfer.
            if index.is_some() {
                if let Ok(seq) = node.send(&to, port::RESOURCE, &file::encode_cancel(id), now) {
                    self.seqs.insert(seq, (id, Part::Cancel));
                }
            }
            self.events.push(FileEvent::Failed {
                peer: to,
                id,
                outgoing: true,
            });
            return;
        }
        match index {
            None => {
                let (to, offer) = (s.to, s.offer.encode());
                if let Ok(seq) = node.send(&to, port::RESOURCE, &offer, now) {
                    self.seqs.insert(seq, (id, Part::Offer));
                }
            }
            Some(i) => {
                s.resend.push_back(i);
                self.pump(node, id, now);
            }
        }
    }

    /// Keep up to [`WINDOW`] chunks of transfer `id` in flight.
    fn pump(&mut self, node: &mut Node, id: u16, now: Micros) {
        let Some(s) = self.sending.get_mut(&id) else {
            return;
        };
        while s.accepted && s.in_flight < WINDOW {
            let index = match s.resend.pop_front() {
                Some(i) => i,
                None if s.next < s.offer.count => {
                    s.next += 1;
                    s.next - 1
                }
                None => break,
            };
            let chunk = file::encode_chunk(id, index, &s.data, ROOM);
            match node.send(&s.to, port::RESOURCE, &chunk, now) {
                Ok(seq) => {
                    s.in_flight += 1;
                    self.seqs.insert(seq, (id, Part::Chunk(index)));
                }
                Err(_) => break,
            }
        }
    }

    /// Incoming transfers that stopped getting chunks are reported failed.
    pub fn expire(&mut self, now: Micros) {
        for (from, id) in self.incoming.expire(now) {
            self.events.push(FileEvent::Failed {
                peer: from,
                id,
                outgoing: false,
            });
        }
    }

    pub fn next_deadline(&self) -> Option<Micros> {
        self.incoming.next_deadline()
    }

    pub fn take_events(&mut self) -> Vec<FileEvent> {
        std::mem::take(&mut self.events)
    }
}
