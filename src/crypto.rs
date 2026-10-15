//! Pairing-code authentication (SPAKE2) and encryption (ChaCha20-Poly1305).
//!
//! The access code never crosses the network: both sides run SPAKE2 with it, and a wrong code
//! simply yields different keys, so the first encrypted message fails to decrypt.

use std::io::{Read, Write};
use std::net::TcpStream;

use anyhow::{Context, Result, anyhow, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::proto::{Hello, PROTOCOL_VERSION};

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

pub fn write_frame(s: &mut impl Write, data: &[u8]) -> Result<()> {
    if data.len() > MAX_FRAME {
        bail!("frame too large");
    }
    let mut buf = Vec::with_capacity(4 + data.len());
    buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
    buf.extend_from_slice(data);
    s.write_all(&buf)?;
    Ok(())
}

pub fn read_frame(s: &mut impl Read) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("frame too large ({len})");
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

/// Normalizes a user-typed access code: uppercase, letters and digits only.
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

pub struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    pub media: [u8; 32],
    pub punch: [u8; 32],
}

fn derive(shared: &[u8], transcript: &[u8]) -> SessionKeys {
    let salt = Sha256::digest(transcript);
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut k = SessionKeys { c2s: [0; 32], s2c: [0; 32], media: [0; 32], punch: [0; 32] };
    hk.expand(b"hop c2s", &mut k.c2s).unwrap();
    hk.expand(b"hop s2c", &mut k.s2c).unwrap();
    hk.expand(b"hop media", &mut k.media).unwrap();
    hk.expand(b"hop punch", &mut k.punch).unwrap();
    k
}

fn transcript(client_hello: &[u8], host_hello: &[u8], client_msg: &[u8], host_msg: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    for part in [client_hello, host_hello, client_msg, host_msg] {
        t.extend_from_slice(&(part.len() as u32).to_be_bytes());
        t.extend_from_slice(part);
    }
    t
}

/// Client side of the handshake. Returns the host's hello and the session keys.
pub fn client_handshake(s: &mut TcpStream, code: &str, me: &Hello) -> Result<(Hello, SessionKeys)> {
    let my_hello = serde_json::to_vec(me)?;
    write_frame(s, &my_hello)?;
    let their_hello_raw = read_frame(s).context("host closed the connection")?;
    let their_hello: Hello = serde_json::from_slice(&their_hello_raw)?;
    if let Some(e) = &their_hello.error {
        return Err(HostRefused(e.clone()).into());
    }
    if their_hello.hop != PROTOCOL_VERSION {
        bail!(
            "{} runs a different version of Hop (protocol v{}, this one is v{}); update both",
            their_hello.name,
            their_hello.hop,
            PROTOCOL_VERSION
        );
    }
    let (spake, msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(normalize_code(code).as_bytes()),
        &Identity::new(b"hop-v1"),
    );
    write_frame(s, &msg)?;
    let their_msg = read_frame(s)?;
    let shared = spake.finish(&their_msg).map_err(|e| anyhow!("key exchange failed: {e:?}"))?;
    let keys = derive(&shared, &transcript(&my_hello, &their_hello_raw, &msg, &their_msg));
    Ok((their_hello, keys))
}

/// The host turned the connection down; the message is meant for the user.
#[derive(Debug)]
pub struct HostRefused(pub String);

impl std::fmt::Display for HostRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostRefused {}

/// Tells a connecting client why it can't connect, then gives up on it.
pub fn host_refuse(s: &mut TcpStream, me: &Hello, why: &str) -> Result<()> {
    let _ = read_frame(s)?;
    let refusal = Hello { error: Some(why.to_string()), ..me.clone() };
    write_frame(s, &serde_json::to_vec(&refusal)?)
}

/// Host side of the handshake.
pub fn host_handshake(s: &mut TcpStream, code: &str, me: &Hello) -> Result<(Hello, SessionKeys)> {
    let their_hello_raw = read_frame(s)?;
    let their_hello: Hello = serde_json::from_slice(&their_hello_raw)?;
    let my_hello = serde_json::to_vec(me)?;
    write_frame(s, &my_hello)?;
    if their_hello.hop != PROTOCOL_VERSION {
        bail!("version mismatch: client speaks v{}", their_hello.hop);
    }
    let their_msg = read_frame(s)?;
    let (spake, msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(normalize_code(code).as_bytes()),
        &Identity::new(b"hop-v1"),
    );
    write_frame(s, &msg)?;
    let shared = spake.finish(&their_msg).map_err(|e| anyhow!("key exchange failed: {e:?}"))?;
    let keys = derive(&shared, &transcript(&their_hello_raw, &my_hello, &their_msg, &msg));
    Ok((their_hello, keys))
}

fn counter_nonce(prefix: u8, counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[0] = prefix;
    n[4..].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&n)
}

/// One direction of an encrypted, ordered stream (TCP control channel).
pub struct StreamCipher {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl StreamCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), counter: 0 }
    }

    pub fn seal(&mut self, pt: &[u8]) -> Vec<u8> {
        let n = counter_nonce(0, self.counter);
        self.counter += 1;
        self.cipher.encrypt(&n, pt).expect("encrypt")
    }

    pub fn open(&mut self, ct: &[u8]) -> Result<Vec<u8>> {
        let n = counter_nonce(0, self.counter);
        self.counter += 1;
        self.cipher.decrypt(&n, ct).map_err(|_| anyhow!("decryption failed (wrong access code?)"))
    }
}

/// Datagram encryption: the nonce comes from a (kind, sequence) pair carried in the clear header.
pub struct PacketCipher {
    cipher: ChaCha20Poly1305,
}

pub const PACKET_HEADER: usize = 12;
pub const PACKET_MAGIC: u8 = 0xB7;
pub const TAG_LEN: usize = 16;

impl PacketCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)) }
    }

    /// Writes header + ciphertext into `out`.
    pub fn seal_into(&self, kind: u8, seq: u64, payload: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.push(PACKET_MAGIC);
        out.push(kind);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&seq.to_le_bytes());
        let header: [u8; PACKET_HEADER] = out[..PACKET_HEADER].try_into().unwrap();
        let ct = self
            .cipher
            .encrypt(&counter_nonce(kind, seq), Payload { msg: payload, aad: &header })
            .expect("encrypt");
        out.extend_from_slice(&ct);
    }

    /// Returns (kind, seq, plaintext).
    pub fn open(&self, pkt: &[u8]) -> Option<(u8, u64, Vec<u8>)> {
        if pkt.len() < PACKET_HEADER + TAG_LEN || pkt[0] != PACKET_MAGIC {
            return None;
        }
        let kind = pkt[1];
        let seq = u64::from_le_bytes(pkt[4..12].try_into().unwrap());
        let pt = self
            .cipher
            .decrypt(&counter_nonce(kind, seq), Payload { msg: &pkt[PACKET_HEADER..], aad: &pkt[..PACKET_HEADER] })
            .ok()?;
        Some((kind, seq, pt))
    }
}

pub fn random_code() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let mut rng = rand::thread_rng();
    (0..6).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn hello(name: &str) -> Hello {
        Hello { hop: PROTOCOL_VERSION, id: name.into(), name: name.into(), os: "test".into(), error: None }
    }

    fn run(host_code: &'static str, client_code: &'static str) -> bool {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let (_, k) = host_handshake(&mut s, host_code, &hello("host")).unwrap();
            let mut tx = StreamCipher::new(&k.s2c);
            write_frame(&mut s, &tx.seal(b"welcome")).unwrap();
        });
        let mut s = TcpStream::connect(addr).unwrap();
        let (_, k) = client_handshake(&mut s, client_code, &hello("client")).unwrap();
        let mut rx = StreamCipher::new(&k.s2c);
        let ok = rx.open(&read_frame(&mut s).unwrap()).map(|m| m == b"welcome").unwrap_or(false);
        h.join().unwrap();
        ok
    }

    #[test]
    fn right_code_works() {
        assert!(run("K7Q-M3P", "k7qm3p"));
    }

    #[test]
    fn wrong_code_fails() {
        assert!(!run("K7QM3P", "K7QM3X"));
    }

    #[test]
    fn packet_roundtrip() {
        let c = PacketCipher::new(&[7; 32]);
        let mut out = Vec::new();
        c.seal_into(1, 42, b"hello", &mut out);
        assert_eq!(c.open(&out).unwrap(), (1, 42, b"hello".to_vec()));
        out[20] ^= 1;
        assert!(c.open(&out).is_none());
    }
}
