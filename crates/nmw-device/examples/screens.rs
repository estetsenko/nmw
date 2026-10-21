//! Render each screen of the device UI to a PNG, scaled up, so it can be
//! reviewed without hardware: `cargo run -p nmw-device --example screens -- out/`

use nmw_core::position::Position;
use nmw_core::Identity;
use nmw_device::app::{Body, Device, Press};
use nmw_device::ui::{draw, FrameBuffer, HEIGHT, WIDTH};

const SCALE: u32 = 4;
const S: u64 = 1_000_000;

fn save(dev: &Device, now: u64, dir: &std::path::Path, name: &str) {
    let mut fb = FrameBuffer::default();
    draw(dev, &mut fb, now).unwrap();
    let (w, h) = (WIDTH * SCALE, HEIGHT * SCALE);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let on = fb.pixels[(y / SCALE) as usize][(x / SCALE) as usize];
            rgb.extend_from_slice(if on {
                &[0xE8, 0xF0, 0xFF]
            } else {
                &[0x08, 0x0A, 0x10]
            });
        }
    }
    let path = dir.join(format!("{name}.png"));
    let mut enc = png::Encoder::new(std::fs::File::create(&path).unwrap(), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    println!("{}", path.display());
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "screens".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut dev = Device::new(Identity::from_seed(&[9; 32]), "Heltec 3f2a", 1, 0);
    save(&dev, 0, &dir, "0-no-messages");

    dev.inject(
        Some("#public"),
        "Sim user 4",
        Body::Text("Testing my new antenna from the hill.".into()),
        10 * S,
    );
    dev.inject(
        None,
        "Evgenii",
        Body::Text("Привет! The Heltec works, see you at the river at 18:00.".into()),
        200 * S,
    );
    dev.inject(
        Some("#public"),
        "Base camp",
        Body::Position(Position {
            lat: 52.3811,
            lon: 4.9032,
            alt_m: 4,
        }),
        300 * S,
    );
    let long = "Long messages are shown page by page: a short press shows the next part, and after the last part the next message. ".repeat(2);
    dev.inject(None, "Info bot", Body::Text(long), 400 * S);
    let now = 460 * S;
    save(&dev, now, &dir, "1-long-message-page-1");
    dev.on_button(Press::Short, now);
    save(&dev, now, &dir, "2-long-message-page-2");
    dev.on_button(Press::Short, now);
    dev.on_button(Press::Short, now);
    save(&dev, now, &dir, "3-position-no-fix");
    dev.on_button(Press::Short, now);
    save(&dev, now, &dir, "4-direct-message");
    dev.on_button(Press::Short, now);
    save(&dev, now, &dir, "5-channel-post");

    dev.on_button(Press::Short, now);
    save(&dev, now, &dir, "6-gps-no-data");
    // A fix near Amsterdam.
    let rmc = |b: &str| {
        let sum = b.bytes().fold(0u8, |a, c| a ^ c);
        format!("${b}*{sum:02X}\r\n")
    };
    dev.on_gps(rmc("GNGGA,101530.00,5222.200,N,00453.400,E,1,09,0.9,3.0,M,46.9,M,,").as_bytes());
    dev.on_gps(rmc("GNRMC,101530.00,A,5222.200,N,00453.400,E,0.1,0.0,211026,,,A").as_bytes());
    save(&dev, now, &dir, "7-gps-fix");
    dev.on_button(Press::Long, now);
    save(&dev, now, &dir, "8-position-shared");
    dev.on_button(Press::Short, now + 4 * S);
    dev.tick(now + 4 * S);
    save(&dev, now + 4 * S, &dir, "9-this-node");
    // Back round to the position, now with a distance.
    for _ in 0..4 {
        dev.on_button(Press::Short, now + 5 * S);
    }
    save(&dev, now + 5 * S, &dir, "10-position-with-distance");
}
