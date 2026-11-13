//! Fyers HSM data-socket wire protocol (binary frames over `wss`).
//!
//! Pure encode/decode, no I/O, so it can be unit-tested with hand-built
//! frames. Layout taken from the official `fyers-apiv3` SDK (`data_ws.py`) and
//! openalgo's `broker/fyers/streaming`; both agree. All integers are
//! big-endian.
//!
//! Requests  : `[len u16][type u8][field count u8] fields...`, a field being
//!             `[id u8][len u16][value]`.
//! Responses : `[len u16][type u8]...`. Type 1 = auth reply, 6 = market data.

use std::collections::HashMap;

/// Channel used for every subscription (the SDK default).
pub const CHANNEL: u8 = 11;

/// "No value" marker used by the feed for missing fields.
const NULL: i32 = i32::MIN;

// Field positions inside a scrip (`sf|...`) record (SDK `map.json` `data_val`).
const S_LTP: usize = 0;
const S_VOL: usize = 1;
const S_BID: usize = 6;
const S_ASK: usize = 7;
const S_OI: usize = 12;
const S_LOW: usize = 13;
const S_HIGH: usize = 14;
const S_OPEN: usize = 19;
const S_PREV: usize = 20;

// Field positions inside an index (`if|...`) record (`index_val`).
const I_LTP: usize = 0;
const I_PREV: usize = 1;
const I_HIGH: usize = 3;
const I_LOW: usize = 4;
const I_OPEN: usize = 5;

// ------------------------------------------------------------- requests ---

/// Authentication frame: the `hsm_key` claim from the access-token JWT.
pub fn auth_frame(hsm_key: &str, source: &str) -> Vec<u8> {
    let (k, s) = (hsm_key.as_bytes(), source.as_bytes());
    let total = 18 + k.len() + s.len();
    let mut b = Vec::with_capacity(total);
    b.extend_from_slice(&((total - 2) as u16).to_be_bytes());
    b.push(1); // request type: auth
    b.push(4); // field count
    b.push(1);
    b.extend_from_slice(&(k.len() as u16).to_be_bytes());
    b.extend_from_slice(k);
    b.push(2);
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(b'P');
    b.push(3);
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(1);
    b.push(4);
    b.extend_from_slice(&(s.len() as u16).to_be_bytes());
    b.extend_from_slice(s);
    b
}

/// Full (`F`) or lite (`L`, LTP only) data mode for the channel. The SDK sends
/// a zero length field here and the server accepts it.
pub fn mode_frame(lite: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(19);
    b.extend_from_slice(&0u16.to_be_bytes());
    b.push(12); // request type: mode
    b.push(2); // field count
    b.push(1);
    b.extend_from_slice(&8u16.to_be_bytes());
    b.extend_from_slice(&(1u64 << CHANNEL).to_be_bytes());
    b.push(2);
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(if lite { b'L' } else { b'F' });
    b
}

fn topics_frame(kind: u8, topics: &[String]) -> Vec<u8> {
    let mut scrips = Vec::new();
    scrips.extend_from_slice(&(topics.len() as u16).to_be_bytes());
    for t in topics {
        let tb = t.as_bytes();
        scrips.push(tb.len().min(255) as u8);
        scrips.extend_from_slice(&tb[..tb.len().min(255)]);
    }
    let mut b = Vec::with_capacity(scrips.len() + 12);
    // The server ignores this length (SDK and openalgo disagree on the
    // formula); openalgo's is used.
    b.extend_from_slice(&((6 + scrips.len()) as u16).to_be_bytes());
    b.push(kind);
    b.push(2); // field count
    b.push(1);
    b.extend_from_slice(&(scrips.len() as u16).to_be_bytes());
    b.extend_from_slice(&scrips);
    b.push(2);
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(CHANNEL);
    b
}

/// Subscribe to topics such as `sf|nse_fo|35003` or `if|nse_cm|Nifty 50`.
pub fn subscribe_frame(topics: &[String]) -> Vec<u8> {
    topics_frame(4, topics)
}

pub fn unsubscribe_frame(topics: &[String]) -> Vec<u8> {
    topics_frame(5, topics)
}

/// Acknowledge data message number `msg_no` (required every `ack_every`
/// data frames, or the server stops sending).
pub fn ack_frame(msg_no: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(11);
    b.extend_from_slice(&9u16.to_be_bytes());
    b.push(3);
    b.push(1);
    b.push(1);
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(&msg_no.to_be_bytes());
    b
}

/// Application-level keepalive, sent every 10 s.
pub fn ping_frame() -> Vec<u8> {
    vec![0, 1, 11]
}

// ------------------------------------------------------------- decoding ---

/// Latest known state of one topic, prices already scaled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tick {
    pub topic: String,
    pub index: bool,
    pub ltp: Option<f64>,
    pub volume: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub oi: Option<f64>,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub prev_close: Option<f64>,
}

/// What an incoming binary frame turned out to be.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Auth reply. `ok` is false when Fyers rejected the key. `ack_every` is
    /// how many data frames may arrive between acknowledgements (0 = none).
    Auth { ok: bool, ack_every: u32 },
    /// Market data with its message number (for acks).
    Data { msg_no: u32, ticks: Vec<Tick> },
    /// Anything else (subscribe/unsubscribe/mode replies, unknown types).
    Other(u8),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Scrip,
    Index,
    Depth,
}

struct TopicState {
    name: String,
    kind: Kind,
    /// `10^precision * multiplier`: prices on the wire are `price * div`.
    div: f64,
    raw: Vec<i32>,
}

impl TopicState {
    fn get(&self, i: usize) -> Option<i32> {
        self.raw.get(i).copied().filter(|v| *v != NULL)
    }

    fn price(&self, i: usize) -> Option<f64> {
        self.get(i).map(|v| v as f64 / self.div)
    }

    fn tick(&self) -> Tick {
        match self.kind {
            Kind::Scrip => Tick {
                topic: self.name.clone(),
                index: false,
                ltp: self.price(S_LTP),
                volume: self.get(S_VOL).map(|v| v as f64),
                bid: self.price(S_BID),
                ask: self.price(S_ASK),
                oi: self.get(S_OI).map(|v| v as f64),
                open: self.price(S_OPEN),
                high: self.price(S_HIGH),
                low: self.price(S_LOW),
                prev_close: self.price(S_PREV),
            },
            Kind::Index => Tick {
                topic: self.name.clone(),
                index: true,
                ltp: self.price(I_LTP),
                open: self.price(I_OPEN),
                high: self.price(I_HIGH),
                low: self.price(I_LOW),
                prev_close: self.price(I_PREV),
                ..Tick::default()
            },
            Kind::Depth => Tick { topic: self.name.clone(), ..Tick::default() },
        }
    }
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8], p: usize) -> Self {
        Self { b, p }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n)?;
        let s = self.b.get(self.p..end)?;
        self.p = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_be_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }
    fn string(&mut self, n: usize) -> Option<String> {
        self.take(n).map(|s| String::from_utf8_lossy(s).into_owned())
    }
}

/// Stateful decoder: a snapshot record names a topic id and carries the
/// scaling; later update records only carry the id and changed fields.
#[derive(Default)]
pub struct Decoder {
    topics: HashMap<u16, TopicState>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget all topic ids (call on reconnect: ids are per connection).
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.topics.clear();
    }

    pub fn decode(&mut self, frame: &[u8]) -> Incoming {
        if frame.len() < 3 {
            return Incoming::Other(0);
        }
        match frame[2] {
            1 => Self::decode_auth(frame),
            6 => self.decode_data(frame),
            t => Incoming::Other(t),
        }
    }

    fn decode_auth(frame: &[u8]) -> Incoming {
        // [len2][type][field count][id][len2][value]...
        let parse = || -> Option<Incoming> {
            let mut c = Cur::new(frame, 4);
            c.u8()?; // field id
            let n = c.u16()? as usize;
            let status = c.string(n)?;
            let mut ack_every = 0;
            if c.u8().is_some() {
                // second field: ack count (u32)
                let n2 = c.u16()? as usize;
                if n2 == 4 {
                    ack_every = c.u32()?;
                }
            }
            Some(Incoming::Auth { ok: status == "K", ack_every })
        };
        parse().unwrap_or(Incoming::Auth { ok: false, ack_every: 0 })
    }

    fn decode_data(&mut self, frame: &[u8]) -> Incoming {
        let mut ticks = Vec::new();
        let mut c = Cur::new(frame, 3);
        let Some(msg_no) = c.u32() else { return Incoming::Other(6) };
        let Some(count) = c.u16() else { return Incoming::Data { msg_no, ticks } };
        for _ in 0..count {
            if self.record(&mut c, &mut ticks).is_none() {
                break; // truncated or unknown record: keep what decoded so far
            }
        }
        Incoming::Data { msg_no, ticks }
    }

    fn record(&mut self, c: &mut Cur, out: &mut Vec<Tick>) -> Option<()> {
        match c.u8()? {
            83 => {
                // Snapshot: [id u16][topic len u8][topic][field count u8]
                // [i32 x count][2 skip][multiplier u16][precision u8]
                // [3 x (len u8, string)].
                let id = c.u16()?;
                let name_len = c.u8()? as usize;
                let name = c.string(name_len)?;
                let n = c.u8()? as usize;
                let mut raw = Vec::with_capacity(n);
                for _ in 0..n {
                    raw.push(c.i32()?);
                }
                c.take(2)?;
                let multiplier = c.u16()?;
                let precision = c.u8()?;
                for _ in 0..3 {
                    let l = c.u8()? as usize;
                    c.take(l)?;
                }
                let kind = if name.starts_with("if|") {
                    Kind::Index
                } else if name.starts_with("dp|") {
                    Kind::Depth
                } else {
                    Kind::Scrip
                };
                let mult = if multiplier == 0 { 1.0 } else { multiplier as f64 };
                let div = 10f64.powi(precision as i32) * mult;
                let st = TopicState { name, kind, div, raw };
                if st.kind != Kind::Depth {
                    out.push(st.tick());
                }
                self.topics.insert(id, st);
                Some(())
            }
            85 => {
                // Update: [id u16][field count u8][i32 x count], applied by
                // position onto the snapshot.
                let id = c.u16()?;
                let n = c.u8()? as usize;
                let st = self.topics.get_mut(&id);
                match st {
                    Some(st) => {
                        let mut changed = false;
                        for i in 0..n {
                            let v = c.i32()?;
                            if v != NULL && st.raw.get(i).copied() != Some(v) {
                                if i >= st.raw.len() {
                                    st.raw.resize(i + 1, NULL);
                                }
                                st.raw[i] = v;
                                changed = true;
                            }
                        }
                        if changed && st.kind != Kind::Depth {
                            out.push(st.tick());
                        }
                    }
                    None => {
                        c.take(n.checked_mul(4)?)?;
                    }
                }
                Some(())
            }
            76 => {
                // Lite update: [id u16][ltp i32]
                let id = c.u16()?;
                let v = c.i32()?;
                if let Some(st) = self.topics.get_mut(&id) {
                    if v != NULL {
                        if st.raw.is_empty() {
                            st.raw.push(NULL);
                        }
                        st.raw[0] = v;
                        out.push(st.tick());
                    }
                }
                Some(())
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(id: u16, name: &str, raw: &[i32], mult: u16, prec: u8) -> Vec<u8> {
        let mut b = vec![83];
        b.extend_from_slice(&id.to_be_bytes());
        b.push(name.len() as u8);
        b.extend_from_slice(name.as_bytes());
        b.push(raw.len() as u8);
        for v in raw {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&mult.to_be_bytes());
        b.push(prec);
        for s in ["NSE", "35003", "SYM"] {
            b.push(s.len() as u8);
            b.extend_from_slice(s.as_bytes());
        }
        b
    }

    fn update(id: u16, vals: &[i32]) -> Vec<u8> {
        let mut b = vec![85];
        b.extend_from_slice(&id.to_be_bytes());
        b.push(vals.len() as u8);
        for v in vals {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b
    }

    fn data_frame(msg_no: u32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut b = vec![0, 0, 6];
        b.extend_from_slice(&msg_no.to_be_bytes());
        b.extend_from_slice(&(records.len() as u16).to_be_bytes());
        for r in records {
            b.extend_from_slice(r);
        }
        b
    }

    fn scrip_raw() -> Vec<i32> {
        let mut raw = vec![NULL; 21];
        raw[S_LTP] = 12345; // 123.45 at precision 2, multiplier 1
        raw[S_VOL] = 5000;
        raw[S_BID] = 12340;
        raw[S_ASK] = 12350;
        raw[S_OI] = 777;
        raw[S_PREV] = 12000;
        raw
    }

    #[test]
    fn auth_frame_layout() {
        let f = auth_frame("KEY", "src");
        assert_eq!(f.len(), 18 + 3 + 3);
        assert_eq!(u16::from_be_bytes([f[0], f[1]]) as usize, f.len() - 2);
        assert_eq!(&f[2..4], &[1, 4]);
        assert_eq!(&f[4..10], &[1, 0, 3, b'K', b'E', b'Y']);
        assert_eq!(&f[10..14], &[2, 0, 1, b'P']);
    }

    #[test]
    fn mode_subscribe_ack_ping_layout() {
        let m = mode_frame(false);
        assert_eq!(m.len(), 19);
        assert_eq!(&m[2..4], &[12, 2]);
        assert_eq!(*m.last().unwrap(), b'F');
        assert_eq!(*mode_frame(true).last().unwrap(), b'L');
        let s = subscribe_frame(&["if|nse_cm|Nifty 50".to_string()]);
        assert_eq!(s[2], 4);
        assert_eq!(&s[s.len() - 4..], &[2, 0, 1, CHANNEL]);
        assert_eq!(unsubscribe_frame(&["x".to_string()])[2], 5);
        assert_eq!(ack_frame(7), vec![0, 9, 3, 1, 1, 0, 4, 0, 0, 0, 7]);
        assert_eq!(ping_frame(), vec![0, 1, 11]);
    }

    #[test]
    fn auth_reply_parsing() {
        let mut d = Decoder::new();
        let mut f = vec![0, 0, 1, 2, 1, 0, 1, b'K', 2, 0, 4];
        f.extend_from_slice(&50u32.to_be_bytes());
        assert_eq!(d.decode(&f), Incoming::Auth { ok: true, ack_every: 50 });
        let bad = vec![0, 0, 1, 1, 1, 0, 1, b'E'];
        assert_eq!(d.decode(&bad), Incoming::Auth { ok: false, ack_every: 0 });
        assert_eq!(d.decode(&[0, 0, 1]), Incoming::Auth { ok: false, ack_every: 0 });
    }

    #[test]
    fn snapshot_then_update_scales_prices() {
        let mut d = Decoder::new();
        let snap = data_frame(1, &[snapshot(5, "sf|nse_fo|35003", &scrip_raw(), 1, 2)]);
        let Incoming::Data { msg_no, ticks } = d.decode(&snap) else { panic!("not data") };
        assert_eq!(msg_no, 1);
        assert_eq!(ticks.len(), 1);
        let t = &ticks[0];
        assert_eq!(t.topic, "sf|nse_fo|35003");
        assert!(!t.index);
        assert_eq!(t.ltp, Some(123.45));
        assert_eq!(t.bid, Some(123.40));
        assert_eq!(t.ask, Some(123.50));
        assert_eq!(t.volume, Some(5000.0));
        assert_eq!(t.oi, Some(777.0));
        assert_eq!(t.prev_close, Some(120.0));
        assert_eq!(t.open, None);

        // Update only the LTP (field 0); everything else is retained.
        let Incoming::Data { ticks, .. } = d.decode(&data_frame(2, &[update(5, &[12400])])) else {
            panic!("not data")
        };
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].ltp, Some(124.0));
        assert_eq!(ticks[0].oi, Some(777.0));

        // Identical value: nothing emitted.
        let Incoming::Data { ticks, .. } = d.decode(&data_frame(3, &[update(5, &[12400])])) else {
            panic!("not data")
        };
        assert!(ticks.is_empty());
    }

    #[test]
    fn index_snapshot_and_lite() {
        let mut d = Decoder::new();
        let raw = vec![2_450_012, 2_440_000, NULL, 2_460_000, 2_430_000, 2_445_000];
        let snap = data_frame(1, &[snapshot(9, "if|nse_cm|Nifty 50", &raw, 1, 2)]);
        let Incoming::Data { ticks, .. } = d.decode(&snap) else { panic!("not data") };
        assert!(ticks[0].index);
        assert_eq!(ticks[0].ltp, Some(24500.12));
        assert_eq!(ticks[0].prev_close, Some(24400.0));
        assert_eq!(ticks[0].high, Some(24600.0));

        let mut lite = vec![76];
        lite.extend_from_slice(&9u16.to_be_bytes());
        lite.extend_from_slice(&2_451_000i32.to_be_bytes());
        let Incoming::Data { ticks, .. } = d.decode(&data_frame(2, &[lite])) else {
            panic!("not data")
        };
        assert_eq!(ticks[0].ltp, Some(24510.0));
    }

    #[test]
    fn multiplier_scales_prices() {
        let mut d = Decoder::new();
        let mut raw = vec![NULL; 21];
        raw[S_LTP] = 10_000;
        let snap = data_frame(1, &[snapshot(1, "sf|nse_fo|1", &raw, 100, 2)]);
        let Incoming::Data { ticks, .. } = d.decode(&snap) else { panic!("not data") };
        assert_eq!(ticks[0].ltp, Some(1.0));
    }

    #[test]
    fn unknown_topic_update_and_truncation_are_safe() {
        let mut d = Decoder::new();
        let Incoming::Data { ticks, .. } = d.decode(&data_frame(1, &[update(42, &[1, 2, 3])])) else {
            panic!("not data")
        };
        assert!(ticks.is_empty());
        // Truncated snapshot must not panic.
        let mut f = data_frame(2, &[snapshot(5, "sf|nse_fo|35003", &scrip_raw(), 1, 2)]);
        f.truncate(f.len() - 9);
        let Incoming::Data { ticks, .. } = d.decode(&f) else { panic!("not data") };
        assert!(ticks.is_empty());
        assert_eq!(d.decode(&[0, 1, 99]), Incoming::Other(99));
    }
}
