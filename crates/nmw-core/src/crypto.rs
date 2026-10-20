//! ChaCha20-Poly1305 (RFC 8439) with a truncated tag, to save airtime.

use alloc::vec::Vec;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20::ChaCha20;
use poly1305::universal_hash::{KeyInit, UniversalHash};
use poly1305::Poly1305;
use subtle::ConstantTimeEq;

/// Bytes of Poly1305 tag carried on air.
pub const TAG_LEN: usize = 8;

fn full_tag(mac_key: &[u8], aad: &[u8], ct: &[u8]) -> [u8; 16] {
    let mut mac = Poly1305::new(poly1305::Key::from_slice(mac_key));
    mac.update_padded(aad);
    mac.update_padded(ct);
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lens[8..].copy_from_slice(&(ct.len() as u64).to_le_bytes());
    mac.update(&[poly1305::Block::clone_from_slice(&lens)]);
    let mut out = [0u8; 16];
    out.copy_from_slice(mac.finalize().as_slice());
    out
}

/// Encrypt `buf` in place and return the 16-byte RFC 8439 tag.
fn seal_full(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; 16] {
    let mut cipher = ChaCha20::new(key.into(), nonce.into());
    // Block 0 keys Poly1305; the message uses blocks from counter 1.
    let mut block0 = [0u8; 64];
    cipher.apply_keystream(&mut block0);
    cipher.apply_keystream(buf);
    full_tag(&block0[..32], aad, buf)
}

/// Encrypt `buf` in place and append a [`TAG_LEN`]-byte tag.
///
/// A (key, nonce) pair must never be reused for a different message.
pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], buf: &mut Vec<u8>) {
    let tag = seal_full(key, nonce, aad, buf);
    buf.extend_from_slice(&tag[..TAG_LEN]);
}

/// Verify and decrypt `buf` (ciphertext followed by tag) in place.
/// On success the tag is removed; on failure `buf` is left unchanged.
pub fn open(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], buf: &mut Vec<u8>) -> bool {
    if buf.len() < TAG_LEN {
        return false;
    }
    let ct_len = buf.len() - TAG_LEN;
    let mut cipher = ChaCha20::new(key.into(), nonce.into());
    let mut block0 = [0u8; 64];
    cipher.apply_keystream(&mut block0);
    let expected = full_tag(&block0[..32], aad, &buf[..ct_len]);
    if !bool::from(expected[..TAG_LEN].ct_eq(&buf[ct_len..])) {
        return false;
    }
    buf.truncate(ct_len);
    cipher.apply_keystream(buf);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc8439_aead_vector() {
        // RFC 8439 section 2.8.2.
        let key: [u8; 32] = hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce: [u8; 12] = hex("070000004041424344454647").try_into().unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let mut buf = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.".to_vec();
        let tag = seal_full(&key, &nonce, &aad, &mut buf);
        assert_eq!(&buf[..16], &hex("d31a8d34648e60db7b86afbc53ef7ec2")[..]);
        assert_eq!(&tag[..], &hex("1ae10b594f09e26a7e902ecbd0600691")[..]);
    }

    #[test]
    fn seal_open_roundtrip_and_tamper() {
        let key = [9u8; 32];
        let nonce = [3u8; 12];
        let mut buf = b"mesh".to_vec();
        seal(&key, &nonce, b"hdr", &mut buf);
        assert_eq!(buf.len(), 4 + TAG_LEN);

        let mut ok = buf.clone();
        assert!(open(&key, &nonce, b"hdr", &mut ok));
        assert_eq!(ok, b"mesh");

        let mut bad_aad = buf.clone();
        assert!(!open(&key, &nonce, b"hdX", &mut bad_aad));
        let mut bad_ct = buf.clone();
        bad_ct[0] ^= 1;
        assert!(!open(&key, &nonce, b"hdr", &mut bad_ct));
        assert_eq!(bad_ct.len(), 4 + TAG_LEN);
    }
}
