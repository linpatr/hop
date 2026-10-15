//! Media datagrams: video frames split into Reed-Solomon protected shards, and PCM audio.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub const KIND_VIDEO: u8 = 1;
pub const KIND_AUDIO: u8 = 2;
pub const KIND_PUNCH: u8 = 3;

/// Bytes of encoded video per datagram. Keeps whole packets (with IP/UDP/crypto headers) under 1400.
pub const MAX_SHARD: usize = 1344;
const VIDEO_HEADER: usize = 20;
const MAX_SHARDS: usize = 30000;

pub struct VideoHeader {
    pub frame_id: u32,
    pub shard_idx: u16,
    pub data_shards: u16,
    pub parity_shards: u16,
    pub keyframe: bool,
    pub frame_len: u32,
    pub host_latency_us: u32,
}

impl VideoHeader {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.shard_idx.to_le_bytes());
        out.extend_from_slice(&self.data_shards.to_le_bytes());
        out.extend_from_slice(&self.parity_shards.to_le_bytes());
        out.push(self.keyframe as u8);
        out.push(0);
        out.extend_from_slice(&self.frame_len.to_le_bytes());
        out.extend_from_slice(&self.host_latency_us.to_le_bytes());
    }

    pub fn parse(b: &[u8]) -> Option<(VideoHeader, &[u8])> {
        if b.len() < VIDEO_HEADER {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Some((
            VideoHeader {
                frame_id: u32_at(0),
                shard_idx: u16_at(4),
                data_shards: u16_at(6),
                parity_shards: u16_at(8),
                keyframe: b[10] & 1 != 0,
                frame_len: u32_at(12),
                host_latency_us: u32_at(16),
            },
            &b[VIDEO_HEADER..],
        ))
    }
}

/// Splits encoded frames into equally sized shards plus parity shards.
pub struct Packetizer {
    frame_id: u32,
    pub fec_percent: u32,
    scratch: Vec<u8>,
}

impl Packetizer {
    pub fn new(fec_percent: u32) -> Self {
        Self { frame_id: 0, fec_percent, scratch: Vec::new() }
    }

    /// Calls `emit` with each datagram payload (before encryption).
    pub fn packetize(&mut self, frame: &[u8], keyframe: bool, host_latency_us: u32, mut emit: impl FnMut(&[u8])) {
        if frame.is_empty() {
            return;
        }
        let id = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);

        let k = frame.len().div_ceil(MAX_SHARD).clamp(1, MAX_SHARDS);
        let mut shard = frame.len().div_ceil(k);
        shard += shard & 1; // the FEC library needs an even shard size
        let mut m = if self.fec_percent == 0 { 0 } else { (k * self.fec_percent as usize).div_ceil(100).max(1) };
        // Keyframes are big and precious: give them a bit more protection.
        if keyframe && m > 0 {
            m = m * 3 / 2 + 1;
        }
        m = m.min(MAX_SHARDS - k);

        let padded = k * shard;
        self.scratch.clear();
        self.scratch.extend_from_slice(frame);
        self.scratch.resize(padded, 0);

        let parity = if m > 0 {
            reed_solomon_simd::encode(k, m, self.scratch.chunks(shard)).unwrap_or_else(|e| {
                log::warn!("fec encode failed: {e}");
                Vec::new()
            })
        } else {
            Vec::new()
        };
        let m = parity.len();

        let mut pkt = Vec::with_capacity(VIDEO_HEADER + shard);
        let mut header = VideoHeader {
            frame_id: id,
            shard_idx: 0,
            data_shards: k as u16,
            parity_shards: m as u16,
            keyframe,
            frame_len: frame.len() as u32,
            host_latency_us,
        };
        for (i, chunk) in self.scratch.chunks(shard).enumerate() {
            header.shard_idx = i as u16;
            pkt.clear();
            header.write(&mut pkt);
            pkt.extend_from_slice(chunk);
            emit(&pkt);
        }
        for (i, p) in parity.iter().enumerate() {
            header.shard_idx = (k + i) as u16;
            pkt.clear();
            header.write(&mut pkt);
            pkt.extend_from_slice(p);
            emit(&pkt);
        }
    }
}

struct Pending {
    k: usize,
    m: usize,
    shard: usize,
    frame_len: usize,
    keyframe: bool,
    host_latency_us: u32,
    buf: Vec<u8>,
    have: Vec<bool>,
    count: usize,
    first_seen: Instant,
}

pub struct CompleteFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub host_latency_us: u32,
    pub recovered: bool,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct ReassemblyStats {
    pub frames: u64,
    pub recovered: u64,
    pub lost: u64,
    pub bytes: u64,
}

/// Rebuilds frames from shards, recovers lost shards with FEC and tracks when the stream is broken
/// (so the client can ask the host for a keyframe).
pub struct Reassembler {
    pending: BTreeMap<u32, Pending>,
    last_delivered: Option<u32>,
    pub need_keyframe: bool,
    pub stats: ReassemblyStats,
    timeout: Duration,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            last_delivered: None,
            need_keyframe: true,
            stats: ReassemblyStats::default(),
            timeout: Duration::from_millis(60),
        }
    }

    fn is_old(&self, id: u32) -> bool {
        match self.last_delivered {
            Some(last) => (id.wrapping_sub(last) as i32) <= 0,
            None => false,
        }
    }

    /// Feeds one datagram payload. Returns a frame ready for the decoder, if any.
    pub fn push(&mut self, payload: &[u8]) -> Option<CompleteFrame> {
        let (h, shard_data) = VideoHeader::parse(payload)?;
        let id = h.frame_id;
        if self.is_old(id) {
            return None;
        }
        self.stats.bytes += payload.len() as u64;
        let k = h.data_shards as usize;
        let m = h.parity_shards as usize;
        let idx = h.shard_idx as usize;
        let shard = shard_data.len();
        if k == 0 || idx >= k + m || shard == 0 || h.frame_len as usize > k * shard {
            return None;
        }
        let p = self.pending.entry(id).or_insert_with(|| Pending {
            k,
            m,
            shard,
            frame_len: h.frame_len as usize,
            keyframe: h.keyframe,
            host_latency_us: h.host_latency_us,
            buf: vec![0; (k + m) * shard],
            have: vec![false; k + m],
            count: 0,
            first_seen: Instant::now(),
        });
        if p.k != k || p.m != m || p.shard != shard || p.have[idx] {
            return None;
        }
        p.buf[idx * shard..(idx + 1) * shard].copy_from_slice(shard_data);
        p.have[idx] = true;
        p.count += 1;
        if p.count < p.k {
            return None;
        }
        let p = self.pending.remove(&id).unwrap();
        let (data, recovered) = Self::assemble(p.k, p.m, p.shard, p.frame_len, &p.buf, &p.have)?;
        self.complete(id, CompleteFrame { data, keyframe: p.keyframe, host_latency_us: p.host_latency_us, recovered })
    }

    fn assemble(k: usize, m: usize, shard: usize, len: usize, buf: &[u8], have: &[bool]) -> Option<(Vec<u8>, bool)> {
        let mut out = buf[..k * shard].to_vec();
        let missing: Vec<usize> = (0..k).filter(|&i| !have[i]).collect();
        if missing.is_empty() {
            out.truncate(len);
            return Some((out, false));
        }
        let originals = (0..k).filter(|&i| have[i]).map(|i| (i, &buf[i * shard..(i + 1) * shard]));
        let recovery = (0..m).filter(|&j| have[k + j]).map(|j| (j, &buf[(k + j) * shard..(k + j + 1) * shard]));
        match reed_solomon_simd::decode(k, m, originals, recovery) {
            Ok(restored) => {
                for (i, data) in restored {
                    out[i * shard..(i + 1) * shard].copy_from_slice(&data);
                }
                out.truncate(len);
                Some((out, true))
            }
            Err(e) => {
                log::warn!("fec decode failed: {e}");
                None
            }
        }
    }

    fn complete(&mut self, id: u32, f: CompleteFrame) -> Option<CompleteFrame> {
        // Anything older that is still pending can no longer complete in order: it's lost.
        let stale: Vec<u32> = self.pending.keys().copied().filter(|&p| (id.wrapping_sub(p) as i32) > 0).collect();
        let mut lost = stale.len() as u64;
        for s in stale {
            self.pending.remove(&s);
        }
        if let Some(last) = self.last_delivered {
            let gap = id.wrapping_sub(last).saturating_sub(1) as u64;
            lost = lost.max(gap);
        }
        if lost > 0 {
            self.stats.lost += lost;
            self.need_keyframe = true;
        }
        self.last_delivered = Some(id);
        if f.recovered {
            self.stats.recovered += 1;
        }
        if f.keyframe {
            self.need_keyframe = false;
        }
        if self.need_keyframe {
            return None;
        }
        self.stats.frames += 1;
        Some(f)
    }

    /// Drops frames that have waited too long; returns true if the stream broke as a result.
    pub fn expire(&mut self) -> bool {
        let now = Instant::now();
        let expired: Vec<u32> =
            self.pending.iter().filter(|(_, p)| now - p.first_seen > self.timeout).map(|(&id, _)| id).collect();
        if expired.is_empty() {
            return false;
        }
        for id in &expired {
            self.pending.remove(id);
        }
        let newest = *expired.iter().max_by_key(|&&id| id.wrapping_sub(expired[0])).unwrap();
        if !self.is_old(newest) {
            self.last_delivered = Some(newest);
        }
        self.stats.lost += expired.len() as u64;
        self.need_keyframe = true;
        true
    }
}

/// Audio datagram payload: frame count + interleaved stereo i16 at 48 kHz.
pub const AUDIO_RATE: u32 = 48000;
pub const AUDIO_FRAMES_PER_PACKET: usize = 240; // 5 ms

pub fn encode_audio(pcm: &[i16], out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(&((pcm.len() / 2) as u16).to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
}

pub fn decode_audio(b: &[u8], out: &mut Vec<i16>) -> bool {
    if b.len() < 2 {
        return false;
    }
    let frames = u16::from_le_bytes([b[0], b[1]]) as usize;
    let data = &b[2..];
    if data.len() < frames * 4 {
        return false;
    }
    out.clear();
    out.extend(data[..frames * 4].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(frame: &[u8], key: bool, p: &mut Packetizer) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        p.packetize(frame, key, 0, |b| v.push(b.to_vec()));
        v
    }

    fn frame(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn roundtrip_no_loss() {
        let mut p = Packetizer::new(10);
        let mut r = Reassembler::new();
        for (i, n) in [50_000usize, 1, 1344, 1345, 200_000].into_iter().enumerate() {
            let f = frame(n, i as u8);
            let mut got = None;
            for pk in packets(&f, i == 0, &mut p) {
                if let Some(c) = r.push(&pk) {
                    got = Some(c);
                }
            }
            assert_eq!(got.expect("frame").data, f);
        }
    }

    #[test]
    fn recovers_lost_shards() {
        let mut p = Packetizer::new(20);
        let mut r = Reassembler::new();
        let f = frame(100_000, 3);
        let pk = packets(&f, true, &mut p);
        let mut got = None;
        for (i, b) in pk.iter().enumerate() {
            if i % 7 == 3 {
                continue; // drop ~14%
            }
            if let Some(c) = r.push(b) {
                got = Some(c);
            }
        }
        let c = got.expect("recovered");
        assert!(c.recovered);
        assert_eq!(c.data, f);
    }

    #[test]
    fn loss_requires_keyframe() {
        let mut p = Packetizer::new(0);
        let mut r = Reassembler::new();
        let f0 = packets(&frame(5000, 0), true, &mut p);
        let f1 = packets(&frame(5000, 1), false, &mut p);
        let f2 = packets(&frame(5000, 2), false, &mut p);
        let f3 = packets(&frame(5000, 3), true, &mut p);
        assert!(f0.iter().filter_map(|b| r.push(b)).count() == 1);
        for b in &f1[1..] {
            assert!(r.push(b).is_none());
        }
        assert!(f2.iter().filter_map(|b| r.push(b)).count() == 0);
        assert!(r.need_keyframe);
        assert!(f3.iter().filter_map(|b| r.push(b)).count() == 1);
        assert!(!r.need_keyframe);
    }

    #[test]
    fn audio_roundtrip() {
        let pcm: Vec<i16> = (0..480).map(|i| (i * 37 - 5000) as i16).collect();
        let mut b = Vec::new();
        encode_audio(&pcm, &mut b);
        let mut out = Vec::new();
        assert!(decode_audio(&b, &mut out));
        assert_eq!(out, pcm);
    }
}
