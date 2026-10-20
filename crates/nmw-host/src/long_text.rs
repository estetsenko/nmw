//! Sends text longer than one packet as parts and puts incoming parts back
//! together, so the rest of the host only sees whole messages.

use nmw_core::node::Micros;
use nmw_core::text::{self, Reassembler};
use nmw_core::{
    port, Address, Channel, ChannelId, Event, Node, SendError, ShortAddr, MAX_DATA, MAX_GROUP_DATA,
};
use std::collections::HashMap;

/// Longest text [`TextLayer::send_text`] takes.
pub const MAX_TEXT: usize = text::max_text_len(MAX_DATA);
/// Longest text [`TextLayer::send_group`] takes.
pub const MAX_GROUP_TEXT: usize = text::max_text_len(MAX_GROUP_DATA);

/// Who a long message is from: channel (none for a direct message), sender,
/// verified address and name. Parts only combine when all of these match.
type PartKey = (Option<ChannelId>, ShortAddr, Option<Address>, String);

struct Multi {
    to: Address,
    remaining: usize,
    failed: bool,
}

#[derive(Default)]
pub struct TextLayer {
    next_msg_id: u16,
    /// Part seq -> seq of the message's first part, which stands for the message.
    part_of: HashMap<u16, u16>,
    multi: HashMap<u16, Multi>,
    incoming: Reassembler<PartKey>,
}

impl TextLayer {
    /// Send `text` to `to`, in parts if needed. The returned seq is the one
    /// later reported in a single `Delivered` (all parts arrived) or `Failed`.
    pub fn send_text(
        &mut self,
        node: &mut Node,
        to: &Address,
        text: &str,
        now: Micros,
    ) -> Result<u16, SendError> {
        if text.len() <= MAX_DATA {
            return node.send(to, port::TEXT, text.as_bytes(), now);
        }
        let parts = self.split(text, MAX_DATA)?;
        let mut first = None;
        for p in &parts {
            let seq = node.send(to, port::TEXT_PART, p, now)?;
            let first = *first.get_or_insert(seq);
            self.part_of.insert(seq, first);
        }
        let first = first.expect("split returns at least one part");
        self.multi.insert(
            first,
            Multi {
                to: *to,
                remaining: parts.len(),
                failed: false,
            },
        );
        Ok(first)
    }

    pub fn send_group(
        &mut self,
        node: &mut Node,
        channel: &Channel,
        text: &str,
        now: Micros,
    ) -> Result<(), SendError> {
        if text.len() <= MAX_GROUP_DATA {
            return node.send_group(channel, port::TEXT, text.as_bytes(), now);
        }
        for p in self.split(text, MAX_GROUP_DATA)? {
            node.send_group(channel, port::TEXT_PART, &p, now)?;
        }
        Ok(())
    }

    fn split(&mut self, text: &str, max: usize) -> Result<Vec<Vec<u8>>, SendError> {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        text::split(text, max, id).ok_or(SendError::TooLarge)
    }

    /// Turn a node event into what the app should see: parts are held until
    /// their message is whole (then reported as one `port::TEXT` message), and
    /// part receipts collapse into one per message.
    pub fn filter(&mut self, ev: Event, now: Micros) -> Option<Event> {
        match ev {
            Event::Message {
                from,
                port: port::TEXT_PART,
                seq,
                data,
            } => {
                let key = (None, from.short(), Some(from), String::new());
                let text = self.incoming.push(key, &data, now)?;
                Some(Event::Message {
                    from,
                    port: port::TEXT,
                    seq,
                    data: text.into_bytes(),
                })
            }
            Event::GroupMessage {
                channel,
                from,
                from_addr,
                name,
                port: port::TEXT_PART,
                data,
            } => {
                let key = (Some(channel), from, from_addr, name);
                let text = self.incoming.push(key.clone(), &data, now)?;
                Some(group_text(key, text))
            }
            Event::Delivered { to, seq } => match self.part_of.remove(&seq) {
                None => Some(Event::Delivered { to, seq }),
                Some(first) => {
                    let m = self.multi.get_mut(&first)?;
                    m.remaining -= 1;
                    if m.remaining > 0 {
                        return None;
                    }
                    let m = self.multi.remove(&first)?;
                    (!m.failed).then_some(Event::Delivered {
                        to: m.to,
                        seq: first,
                    })
                }
            },
            Event::Failed { to, seq } => match self.part_of.remove(&seq) {
                None => Some(Event::Failed { to, seq }),
                Some(first) => {
                    let m = self.multi.get_mut(&first)?;
                    m.remaining -= 1;
                    let report = !m.failed;
                    m.failed = true;
                    if m.remaining == 0 {
                        self.multi.remove(&first);
                    }
                    report.then_some(Event::Failed { to, seq: first })
                }
            },
            other => Some(other),
        }
    }

    /// Messages that stopped receiving parts, released with gaps marked.
    pub fn expire(&mut self, now: Micros) -> Vec<Event> {
        self.incoming
            .expire(now)
            .into_iter()
            .map(|(key, text)| match key {
                (None, _, Some(from), _) => Event::Message {
                    from,
                    port: port::TEXT,
                    seq: 0,
                    data: text.into_bytes(),
                },
                key => group_text(key, text),
            })
            .collect()
    }

    pub fn next_deadline(&self) -> Option<Micros> {
        self.incoming.next_deadline()
    }
}

fn group_text(key: PartKey, text: String) -> Event {
    let (channel, from, from_addr, name) = key;
    Event::GroupMessage {
        channel: channel.expect("group parts carry a channel"),
        from,
        from_addr,
        name,
        port: port::TEXT,
        data: text.into_bytes(),
    }
}
