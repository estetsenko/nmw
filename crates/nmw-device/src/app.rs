//! The standalone device: a [`Node`] plus what the screen shows (received
//! messages, GPS, this node's details) and what the one button does.
//!
//! The board loop feeds radio frames, GPS bytes and button presses in, sends
//! what [`Device::poll_transmit`] returns, and redraws with
//! [`crate::ui::draw`] when [`Device::take_redraw`] says so.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use nmw_core::node::Micros;
use nmw_core::position::Position;
use nmw_core::text::Reassembler;
use nmw_core::{
    port, Address, Channel, ChannelId, Event, Identity, Node, NodeConfig, Role, ShortAddr,
};

use crate::gps::Gps;

pub const PUBLIC_CHANNEL: &str = "#public";
/// Messages kept for the screen; older ones are dropped.
pub const INBOX_LEN: usize = 30;
const S: Micros = 1_000_000;
/// First announce soon after boot, then this often.
const FIRST_ANNOUNCE: Micros = 3 * S;
const ANNOUNCE_EVERY: Micros = 15 * 60 * S;
/// The screen turns off after this long without a button press or message.
pub const SCREEN_TIMEOUT: Micros = 60 * S;
/// A press held at least this long is a long press.
pub const LONG_PRESS: Micros = 800_000;
/// Toasts ("Position sent") stay this long.
const TOAST: Micros = 3 * S;

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Text(String),
    Position(Position),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Sender's announced name, or "Node xxxxxxxx" / "Unverified xxxxxxxx".
    pub from: String,
    /// Channel name for a channel post, `None` for a direct message.
    pub channel: Option<String>,
    pub body: Body,
    pub at: Micros,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Short,
    Long,
}

/// What the screen shows. Short presses step through the messages (newest
/// first, a long one page by page), then GPS, then this node, and around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Message `index` (0 = newest), text page `page`.
    Message {
        index: usize,
        page: usize,
    },
    NoMessages,
    Gps,
    Node,
}

pub struct Device {
    pub node: Node,
    name: String,
    public: Channel,
    public_id: ChannelId,
    parts: Reassembler<(Option<ChannelId>, ShortAddr)>,
    inbox: VecDeque<Message>,
    pub gps: Gps,
    frame: Frame,
    /// Messages that arrived since the user last looked at the newest one.
    unread: usize,
    screen_on: bool,
    last_activity: Micros,
    next_announce: Micros,
    toast: Option<(String, Micros)>,
    redraw: bool,
}

impl Device {
    pub fn new(identity: Identity, name: &str, rng_seed: u64, now: Micros) -> Device {
        let mut cfg = NodeConfig::new(Role::Client, name);
        cfg.rng_seed = rng_seed;
        let mut node = Node::new(identity, cfg, now);
        let public = Channel::public(PUBLIC_CHANNEL);
        let public_id = node.join_channel(&public);
        Device {
            node,
            name: name.into(),
            public,
            public_id,
            parts: Reassembler::default(),
            inbox: VecDeque::new(),
            gps: Gps::default(),
            frame: Frame::NoMessages,
            unread: 0,
            screen_on: true,
            last_activity: now,
            next_announce: now + FIRST_ANNOUNCE,
            toast: None,
            redraw: true,
        }
    }

    pub fn node_name(&self) -> &str {
        &self.name
    }

    pub fn inbox(&self) -> &VecDeque<Message> {
        &self.inbox
    }

    pub fn frame(&self) -> Frame {
        self.frame
    }

    pub fn unread(&self) -> usize {
        self.unread
    }

    pub fn screen_on(&self) -> bool {
        self.screen_on
    }

    pub fn toast(&self) -> Option<&str> {
        self.toast.as_ref().map(|(t, _)| t.as_str())
    }

    /// Whether the screen needs drawing again; clears the flag.
    pub fn take_redraw(&mut self) -> bool {
        core::mem::take(&mut self.redraw)
    }

    pub fn on_frame(&mut self, frame: &[u8], snr_db: f32, now: Micros) {
        self.node.handle_frame(frame, snr_db, now);
        self.collect(now);
    }

    pub fn on_gps(&mut self, bytes: &[u8]) {
        if self.gps.feed(bytes) && self.frame == Frame::Gps {
            self.redraw = true;
        }
    }

    pub fn poll_transmit(&mut self, now: Micros) -> Option<Vec<u8>> {
        self.node.poll_transmit(now)
    }

    /// Timers: retries, announces, part timeouts, the screen and toasts.
    pub fn tick(&mut self, now: Micros) {
        self.node.tick(now);
        if now >= self.next_announce {
            self.node.announce(now);
            self.next_announce = now + ANNOUNCE_EVERY;
        }
        for ((channel, from), text) in self.parts.expire(now) {
            let name = self.short_name(&from);
            self.push(channel, name, Body::Text(text), now);
        }
        self.collect(now);
        if self.screen_on && now.saturating_sub(self.last_activity) >= SCREEN_TIMEOUT {
            self.screen_on = false;
            self.redraw = true;
        }
        if self.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.toast = None;
            self.redraw = true;
        }
    }

    pub fn on_button(&mut self, press: Press, now: Micros) {
        self.last_activity = now;
        self.redraw = true;
        if !self.screen_on {
            // The first press only wakes the screen, on the newest message.
            self.screen_on = true;
            self.frame = self.first_frame();
            self.unread = 0;
            return;
        }
        match press {
            Press::Short => self.frame = self.next_frame(),
            Press::Long => self.action(now),
        }
        if matches!(self.frame, Frame::Message { index: 0, .. }) {
            self.unread = 0;
        }
    }

    fn first_frame(&self) -> Frame {
        if self.inbox.is_empty() {
            Frame::NoMessages
        } else {
            Frame::Message { index: 0, page: 0 }
        }
    }

    fn next_frame(&self) -> Frame {
        match self.frame {
            Frame::Message { index, page } if page + 1 < crate::ui::pages(self, index) => {
                Frame::Message {
                    index,
                    page: page + 1,
                }
            }
            Frame::Message { index, .. } if index + 1 < self.inbox.len() => Frame::Message {
                index: index + 1,
                page: 0,
            },
            Frame::Message { .. } | Frame::NoMessages => Frame::Gps,
            Frame::Gps => Frame::Node,
            Frame::Node => self.first_frame(),
        }
    }

    /// Long press: share the position on the GPS screen, announce on the
    /// node screen, jump back to the newest message elsewhere.
    fn action(&mut self, now: Micros) {
        let toast = match self.frame {
            Frame::Gps => match self.gps.fix() {
                Some(fix) => {
                    let data = fix.position().encode();
                    match self
                        .node
                        .send_group(&self.public, port::POSITION, &data, now)
                    {
                        Ok(()) => "Position sent".into(),
                        Err(_) => "Could not send".into(),
                    }
                }
                None => "No GPS fix yet".into(),
            },
            Frame::Node => {
                self.node.announce(now);
                "Announced".into()
            }
            _ => {
                self.frame = self.first_frame();
                return;
            }
        };
        self.toast = Some((toast, now + TOAST));
    }

    fn collect(&mut self, now: Micros) {
        while let Some(ev) = self.node.poll_event() {
            match ev {
                Event::Message {
                    from, port, data, ..
                } => {
                    let name = self.contact_name(&from);
                    self.on_data(None, from.short(), Some(name), port, &data, now);
                }
                Event::GroupMessage {
                    channel,
                    from,
                    from_addr,
                    name,
                    port,
                    data,
                } => {
                    let name = match from_addr {
                        Some(_) if !name.is_empty() => Some(name),
                        Some(a) => Some(format!("Node {}", hex(&a.0[..4]))),
                        None => None,
                    };
                    self.on_data(Some(channel), from, name, port, &data, now);
                }
                _ => {}
            }
        }
    }

    fn on_data(
        &mut self,
        channel: Option<ChannelId>,
        from: ShortAddr,
        name: Option<String>,
        port: u16,
        data: &[u8],
        now: Micros,
    ) {
        let body = match port {
            port::TEXT => Body::Text(String::from_utf8_lossy(data).into()),
            port::TEXT_PART => match self.parts.push((channel, from), data, now) {
                Some(text) => Body::Text(text),
                None => return,
            },
            port::POSITION => match Position::decode(data) {
                Some(p) => Body::Position(p),
                None => return,
            },
            port::RESOURCE => match nmw_core::file::decode(data) {
                Some(nmw_core::file::Msg::Offer(o)) => {
                    Body::Text(format!("[file {}, {} bytes]", o.name, o.size))
                }
                _ => return,
            },
            _ => return,
        };
        let from = name.unwrap_or_else(|| format!("Unverified {}", hex(&from.0)));
        self.push(channel, from, body, now);
    }

    fn push(&mut self, channel: Option<ChannelId>, from: String, body: Body, now: Micros) {
        let channel = channel.map(|c| {
            if c == self.public_id {
                PUBLIC_CHANNEL.into()
            } else {
                format!("#{}", hex(&c.0))
            }
        });
        self.add(channel, from, body, now);
    }

    /// Put a message in the inbox as if it had arrived (for screenshots and
    /// tests).
    pub fn inject(&mut self, channel: Option<&str>, from: &str, body: Body, now: Micros) {
        self.add(channel.map(String::from), from.into(), body, now);
    }

    fn add(&mut self, channel: Option<String>, from: String, body: Body, now: Micros) {
        self.inbox.push_front(Message {
            from,
            channel,
            body,
            at: now,
        });
        self.inbox.truncate(INBOX_LEN);
        match self.frame {
            // On the newest message, or the screen was off: show the new one.
            Frame::Message { index: 0, page: 0 } | Frame::NoMessages => {
                self.frame = Frame::Message { index: 0, page: 0 }
            }
            _ if !self.screen_on => self.frame = Frame::Message { index: 0, page: 0 },
            // Reading an older one: keep it on screen and count the new one.
            Frame::Message { index, page } => {
                self.frame = Frame::Message {
                    index: (index + 1).min(self.inbox.len() - 1),
                    page,
                };
                self.unread += 1;
            }
            Frame::Gps | Frame::Node => self.unread += 1,
        }
        // A new message wakes the screen.
        self.screen_on = true;
        self.last_activity = now;
        self.redraw = true;
    }

    fn contact_name(&self, addr: &Address) -> String {
        self.node
            .contacts()
            .find(|(a, _)| a == addr)
            .map(|(_, n)| n)
            .filter(|n| !n.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("Node {}", hex(&addr.0[..4])))
    }

    fn short_name(&self, short: &ShortAddr) -> String {
        self.node
            .contacts()
            .find(|(a, _)| a.short() == *short)
            .map(|(_, n)| String::from(n))
            .unwrap_or_else(|| format!("Node {}", hex(&short.0)))
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Device {
        Device::new(Identity::from_seed(&[3; 32]), "Heltec", 1, 0)
    }

    fn text(d: &mut Device, from: &str, t: &str, now: Micros) {
        d.push(None, from.into(), Body::Text(t.into()), now);
    }

    #[test]
    fn buttons_walk_messages_then_gps_then_node() {
        let mut d = device();
        assert_eq!(d.frame(), Frame::NoMessages);
        d.on_button(Press::Short, 1);
        assert_eq!(d.frame(), Frame::Gps);
        d.on_button(Press::Short, 2);
        assert_eq!(d.frame(), Frame::Node);
        d.on_button(Press::Short, 3);
        assert_eq!(d.frame(), Frame::NoMessages);

        text(&mut d, "Alice", "one", 4);
        text(&mut d, "Bob", "two", 5);
        assert_eq!(d.frame(), Frame::Message { index: 0, page: 0 });
        assert_eq!(d.inbox()[0].from, "Bob");
        d.on_button(Press::Short, 7);
        assert_eq!(d.frame(), Frame::Message { index: 1, page: 0 });
        d.on_button(Press::Short, 8);
        assert_eq!(d.frame(), Frame::Gps);
        d.on_button(Press::Long, 9);
        assert_eq!(d.toast(), Some("No GPS fix yet"));
        d.on_button(Press::Short, 10);
        d.on_button(Press::Long, 11);
        assert_eq!(d.toast(), Some("Announced"));
        d.tick(11 + TOAST);
        assert_eq!(d.toast(), None);
    }

    #[test]
    fn reading_an_older_message_stays_put_when_new_ones_arrive() {
        let mut d = device();
        text(&mut d, "A", "1", 1);
        text(&mut d, "B", "2", 2);
        d.on_button(Press::Short, 3); // to the older one
        assert_eq!(d.frame(), Frame::Message { index: 1, page: 0 });
        text(&mut d, "C", "3", 4);
        assert_eq!(d.frame(), Frame::Message { index: 2, page: 0 });
        assert_eq!(d.inbox()[2].from, "A");
        assert_eq!(d.unread(), 1);
        d.on_button(Press::Long, 5); // back to the newest
        assert_eq!(d.frame(), Frame::Message { index: 0, page: 0 });
        assert_eq!(d.unread(), 0);
    }

    #[test]
    fn screen_sleeps_and_a_message_or_press_wakes_it() {
        let mut d = device();
        d.tick(SCREEN_TIMEOUT);
        assert!(!d.screen_on());
        d.on_button(Press::Short, SCREEN_TIMEOUT + 1);
        assert!(d.screen_on());
        assert_eq!(
            d.frame(),
            Frame::NoMessages,
            "the waking press doesn't step"
        );
        d.tick(3 * SCREEN_TIMEOUT);
        assert!(!d.screen_on());
        text(&mut d, "A", "wake up", 3 * SCREEN_TIMEOUT + 1);
        assert!(d.screen_on());
        for i in 0..40 {
            text(&mut d, "A", "x", 4 * SCREEN_TIMEOUT + i);
        }
        assert_eq!(d.inbox().len(), INBOX_LEN);
    }

    #[test]
    fn messages_and_positions_arrive_over_the_air() {
        // Two devices next to each other: B posts to #public, then a position.
        let mut a = device();
        let mut b = Device::new(Identity::from_seed(&[4; 32]), "Base", 2, 0);
        let mut t = 0;
        let air = |a: &mut Device, b: &mut Device, t: &mut Micros, until: Micros| {
            while *t < until {
                *t += 10_000;
                a.tick(*t);
                b.tick(*t);
                while let Some(f) = a.poll_transmit(*t) {
                    b.on_frame(&f, 5.0, *t);
                }
                while let Some(f) = b.poll_transmit(*t) {
                    a.on_frame(&f, 5.0, *t);
                }
            }
        };
        air(&mut a, &mut b, &mut t, 10 * S); // announces
        b.node
            .send_group(
                &b.public.clone(),
                port::TEXT,
                "hello from base".as_bytes(),
                t,
            )
            .unwrap();
        let long = "word ".repeat(60);
        let parts = nmw_core::text::split(&long, nmw_core::MAX_GROUP_DATA, 1).unwrap();
        for p in &parts {
            b.node
                .send_group(&b.public.clone(), port::TEXT_PART, p, t)
                .unwrap();
        }
        let pos = Position {
            lat: 48.1,
            lon: 11.5,
            alt_m: 500,
        };
        b.node
            .send_group(&b.public.clone(), port::POSITION, &pos.encode(), t)
            .unwrap();
        air(&mut a, &mut b, &mut t, 40 * S);
        let got: Vec<_> = a.inbox().iter().rev().cloned().collect();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0].from, "Base");
        assert_eq!(got[0].channel.as_deref(), Some(PUBLIC_CHANNEL));
        assert_eq!(got[0].body, Body::Text("hello from base".into()));
        assert_eq!(got[1].body, Body::Text(long));
        assert!(matches!(got[2].body, Body::Position(p) if (p.lat - 48.1).abs() < 1e-6));
    }
}
