//! The 128x64 monochrome screen: a header bar, then the current frame.
//! Text uses a 6x10 font with ASCII and Cyrillic (ISO 8859-5); other
//! characters show as `?`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use embedded_graphics::mono_font::iso_8859_5::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use nmw_core::node::Micros;
use nmw_core::position::Position;

use crate::app::{hex, Body, Device, Frame, PUBLIC_CHANNEL};

pub const WIDTH: u32 = 128;
pub const HEIGHT: u32 = 64;
const COLS: usize = 21;
const LINE: i32 = 10;
/// Text lines under the header and the sender line.
const BODY_LINES: usize = 4;

/// Draw the device's current frame.
pub fn draw<D>(dev: &Device, d: &mut D, now: Micros) -> Result<(), D::Error>
where
    D: DrawTarget<Color = BinaryColor>,
{
    d.clear(BinaryColor::Off)?;
    if !dev.screen_on() {
        return Ok(());
    }
    let on = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let off = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
    let (title, right, lines) = frame_text(dev, now);
    Rectangle::new(Point::zero(), Size::new(WIDTH, 11))
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(d)?;
    Text::with_baseline(
        &fit(&title, COLS - right.chars().count() - 1),
        Point::new(1, 0),
        off,
        Baseline::Top,
    )
    .draw(d)?;
    let rw = right.chars().count() as i32 * 6;
    Text::with_baseline(
        &right,
        Point::new(WIDTH as i32 - rw - 1, 0),
        off,
        Baseline::Top,
    )
    .draw(d)?;
    for (i, l) in lines.iter().enumerate() {
        Text::with_baseline(l, Point::new(0, 13 + i as i32 * LINE), on, Baseline::Top).draw(d)?;
    }
    if let Some(t) = dev.toast() {
        let t = fit(t, COLS - 1);
        let w = t.chars().count() as u32 * 6 + 6;
        let x = (WIDTH - w) as i32 / 2;
        let r = Rectangle::new(Point::new(x, 40), Size::new(w, 14));
        r.into_styled(PrimitiveStyle::with_fill(BinaryColor::Off))
            .draw(d)?;
        r.into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
            .draw(d)?;
        Text::with_baseline(&t, Point::new(x + 3, 42), on, Baseline::Top).draw(d)?;
    }
    Ok(())
}

/// Screen pages message `index` takes.
pub fn pages(dev: &Device, index: usize) -> usize {
    dev.inbox().get(index).map_or(1, |m| {
        body_lines(&m.body, dev).len().div_ceil(BODY_LINES).max(1)
    })
}

/// Header title, header right side, and up to five body lines.
fn frame_text(dev: &Device, now: Micros) -> (String, String, Vec<String>) {
    let badge = match dev.unread() {
        0 => String::new(),
        n => format!("+{n} "),
    };
    match dev.frame() {
        Frame::NoMessages => (
            "Messages".into(),
            badge,
            alloc::vec![
                "No messages yet.".into(),
                String::new(),
                format!("Listening on {PUBLIC_CHANNEL}"),
                "Press: next screen".into(),
            ],
        ),
        Frame::Message { index, page } => {
            let Some(m) = dev.inbox().get(index) else {
                return ("Messages".into(), badge, Vec::new());
            };
            let title = format!(
                "{} {}/{}",
                m.channel.as_deref().unwrap_or("Direct"),
                index + 1,
                dev.inbox().len()
            );
            let all = body_lines(&m.body, dev);
            let pages = all.len().div_ceil(BODY_LINES).max(1);
            let page = page.min(pages - 1);
            let mut right = format!("{badge}{}", ago(now.saturating_sub(m.at)));
            if pages > 1 {
                right = format!("{badge}p{}/{pages}", page + 1);
            }
            let mut lines = alloc::vec![fit(&m.from, COLS)];
            lines.extend(all.into_iter().skip(page * BODY_LINES).take(BODY_LINES));
            (title, right, lines)
        }
        Frame::Gps => {
            let lines = match dev.gps.fix() {
                Some(f) => alloc::vec![
                    format!(
                        "{} sats  {:02}:{:02}:{:02} UTC",
                        f.satellites, f.utc.0, f.utc.1, f.utc.2
                    ),
                    format!("Lat {}", degrees(f.lat, 'N', 'S')),
                    format!("Lon {}", degrees(f.lon, 'E', 'W')),
                    format!("Alt {:.0} m", f.alt_m),
                    "Hold: share position".into(),
                ],
                None if dev.gps.sentences == 0 => alloc::vec![
                    "No data from GPS.".into(),
                    "Is the module".into(),
                    "connected?".into(),
                ],
                None => alloc::vec![
                    "Searching...".into(),
                    format!("{} satellites", dev.gps.satellites()),
                    String::new(),
                    "Needs open sky.".into(),
                ],
            };
            ("GPS".into(), badge, lines)
        }
        Frame::Node => {
            let addr = hex(&dev.node.address().0);
            let lines = alloc::vec![
                fit(dev.node_name(), COLS),
                addr[..16].into(),
                addr[16..].into(),
                format!("{} contacts", dev.node.contacts().count()),
                "Hold: announce".into(),
            ];
            ("This node".into(), badge, lines)
        }
    }
}

fn body_lines(body: &Body, dev: &Device) -> Vec<String> {
    match body {
        Body::Text(t) => wrap(t, COLS),
        Body::Position(p) => {
            let mut v = alloc::vec![
                "Shared a position:".into(),
                format!("{} {}", degrees(p.lat, 'N', 'S'), degrees(p.lon, 'E', 'W')),
            ];
            if let Some(me) = dev.gps.fix() {
                let (km, dir) = distance(&me.position(), p);
                v.push(if km < 1.0 {
                    format!("{:.0} m {dir} of you", km * 1000.0)
                } else {
                    format!("{km:.1} km {dir} of you")
                });
            } else {
                v.push(format!("Alt {} m", p.alt_m));
            }
            v
        }
    }
}

/// `48.11730N` style, 5 decimals (about a metre).
fn degrees(v: f64, pos: char, neg: char) -> String {
    format!("{:.5}{}", libm::fabs(v), if v >= 0.0 { pos } else { neg })
}

fn ago(us: Micros) -> String {
    let s = us / 1_000_000;
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Great-circle distance in km and the compass direction from `a` to `b`.
pub fn distance(a: &Position, b: &Position) -> (f64, &'static str) {
    let r = |d: f64| d.to_radians();
    let (la1, la2, dlon) = (r(a.lat), r(b.lat), r(b.lon - a.lon));
    let sq = |x: f64| x * x;
    let h = sq(libm::sin((la2 - la1) / 2.0))
        + libm::cos(la1) * libm::cos(la2) * sq(libm::sin(dlon / 2.0));
    let km = 2.0 * 6371.0 * libm::asin(libm::sqrt(h.min(1.0)));
    let y = libm::sin(dlon) * libm::cos(la2);
    let x = libm::cos(la1) * libm::sin(la2) - libm::sin(la1) * libm::cos(la2) * libm::cos(dlon);
    let bearing = (libm::atan2(y, x).to_degrees() + 360.0) % 360.0;
    const DIRS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    (km, DIRS[((bearing + 22.5) / 45.0) as usize % 8])
}

/// Cut to `n` characters, ending in `~` if anything was cut.
fn fit(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.into();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('~');
    out
}

/// Word-wrap to lines of at most `cols` characters; words longer than a line
/// are split. Newlines in the text start new lines.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut len = 0;
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            let mut word: Vec<char> = word.chars().collect();
            while !word.is_empty() {
                let room = if len == 0 {
                    cols
                } else {
                    cols.saturating_sub(len + 1)
                };
                if word.len() <= room {
                    if len > 0 {
                        line.push(' ');
                        len += 1;
                    }
                    line.extend(word.iter());
                    len += word.len();
                    break;
                }
                if len > 0 {
                    lines.push(core::mem::take(&mut line));
                    len = 0;
                    continue;
                }
                let rest = word.split_off(cols);
                lines.push(word.into_iter().collect());
                word = rest;
            }
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) && lines.len() > 1 {
        lines.pop();
    }
    lines
}

/// A 1-bit frame buffer, for tests and for rendering screenshots on a host.
pub struct FrameBuffer {
    pub pixels: [[bool; WIDTH as usize]; HEIGHT as usize],
}

impl Default for FrameBuffer {
    fn default() -> Self {
        FrameBuffer {
            pixels: [[false; WIDTH as usize]; HEIGHT as usize],
        }
    }
}

impl OriginDimensions for FrameBuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH, HEIGHT)
    }
}

impl DrawTarget for FrameBuffer {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<BinaryColor>>,
    {
        for Pixel(p, c) in pixels {
            if (0..WIDTH as i32).contains(&p.x) && (0..HEIGHT as i32).contains(&p.y) {
                self.pixels[p.y as usize][p.x as usize] = c.is_on();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Body, Press};
    use nmw_core::Identity;

    #[test]
    fn wraps_words_and_splits_long_ones() {
        assert_eq!(wrap("hello mesh world", 10), ["hello mesh", "world"]);
        assert_eq!(wrap("abcdefghijklmnop", 6), ["abcdef", "ghijkl", "mnop"]);
        assert_eq!(wrap("a\nb", 6), ["a", "b"]);
        assert_eq!(
            wrap("Привет всем на mesh", 10),
            ["Привет", "всем на", "mesh"]
        );
        assert_eq!(wrap("", 6), [""]);
    }

    #[test]
    fn distance_and_direction() {
        let a = Position {
            lat: 52.0,
            lon: 4.0,
            alt_m: 0,
        };
        let b = Position {
            lat: 52.0,
            lon: 4.1,
            alt_m: 0,
        };
        let (km, dir) = distance(&a, &b);
        assert!((km - 6.85).abs() < 0.05, "{km}");
        assert_eq!(dir, "E");
        assert_eq!(distance(&b, &a).1, "W");
        let c = Position {
            lat: 52.1,
            lon: 4.1,
            alt_m: 0,
        };
        assert_eq!(distance(&a, &c).1, "NE");
        assert_eq!(fit("abcdef", 4), "abc~");
    }

    #[test]
    fn long_messages_get_pages() {
        let mut dev = Device::new(Identity::from_seed(&[5; 32]), "Heltec", 1, 0);
        let long = "word ".repeat(40);
        dev.inject(None, "Alice", Body::Text(long), 0);
        let mut fb = FrameBuffer::default();
        draw(&dev, &mut fb, 0).unwrap();
        assert!(fb.pixels.iter().flatten().any(|&p| p));
        dev.on_button(Press::Short, 1);
        assert_eq!(dev.frame(), Frame::Message { index: 0, page: 1 });
        let (_, right, _) = frame_text(&dev, 1);
        assert!(right.ends_with("p2/3"), "{right}");
    }
}
