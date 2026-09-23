//! A standard Noise channel; the relay never receives the pre-shared key.
//! Trust comes from delivering separate role files through an existing trusted
//! administrative channel, NOT from accepting keys advertised by the relay.
use crate::protocol::MAX_FRAME;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use snow::{
    Builder, HandshakeState, TransportState,
    resolvers::{DefaultResolver, FallbackResolver, RingResolver},
};

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
const CHUNK: usize = 60_000;
const NOISE_LIMIT: usize = 65_535;
const HANDSHAKE_LIMIT: usize = 256;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Init { data: String },
    Response { data: String },
    Data { data: String },
    RelayEvent { code: String },
}
fn key(hex: &str) -> Result<[u8; 32]> {
    ensure!(
        hex.len() == 64 && hex.is_ascii(),
        "invalid channel key encoding"
    );
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
            .context("invalid channel key encoding")?;
    }
    Ok(bytes)
}
fn handshake(hex: &str, prologue: &[u8], initiator: bool) -> Result<HandshakeState> {
    let key = key(hex)?;
    // Ring supplies RNG, AEAD and SHA; Snow's existing primitive
    // curve25519-dalek supplies X25519 (RingResolver itself has no DH provider).
    let resolver = FallbackResolver::new(Box::new(RingResolver), Box::new(DefaultResolver));
    let builder = Builder::with_resolver(PATTERN.parse()?, Box::new(resolver))
        .psk(0, &key)?
        .prologue(prologue)?;
    if initiator {
        Ok(builder.build_initiator()?)
    } else {
        Ok(builder.build_responder()?)
    }
}
fn decode(data: &str, max: usize) -> Result<Vec<u8>> {
    ensure!(
        data.len() <= max.div_ceil(3) * 4,
        "encrypted frame exceeds limit"
    );
    let decoded = STANDARD
        .decode(data)
        .context("invalid encrypted frame encoding")?;
    ensure!(
        !decoded.is_empty() && decoded.len() <= max,
        "invalid encrypted frame size"
    );
    Ok(decoded)
}

#[cfg(any(feature = "controller", test))]
pub struct Initiator(HandshakeState);
#[cfg(any(feature = "controller", test))]
impl Initiator {
    pub fn new(key: &str, prologue: &[u8]) -> Result<(Self, Frame)> {
        let mut state = handshake(key, prologue, true)?;
        let mut out = [0; HANDSHAKE_LIMIT];
        let n = state.write_message(&[], &mut out)?;
        Ok((
            Self(state),
            Frame::Init {
                data: STANDARD.encode(&out[..n]),
            },
        ))
    }
    pub fn finish(mut self, frame: Frame) -> Result<Channel> {
        let Frame::Response { data } = frame else {
            bail!("expected Noise response");
        };
        let mut payload = [0; HANDSHAKE_LIMIT];
        let n = self
            .0
            .read_message(&decode(&data, HANDSHAKE_LIMIT)?, &mut payload)
            .context("Noise authentication failed")?;
        ensure!(
            n == 0 && self.0.is_handshake_finished(),
            "unexpected handshake payload/state"
        );
        Ok(Channel {
            state: self.0.into_transport_mode()?,
            pending: Vec::new(),
        })
    }
}
pub fn respond(key: &str, prologue: &[u8], frame: Frame) -> Result<(Channel, Frame)> {
    let Frame::Init { data } = frame else {
        bail!("expected Noise init");
    };
    let mut state = handshake(key, prologue, false)?;
    let mut payload = [0; HANDSHAKE_LIMIT];
    let n = state
        .read_message(&decode(&data, HANDSHAKE_LIMIT)?, &mut payload)
        .context("Noise authentication failed")?;
    ensure!(n == 0, "unexpected handshake payload");
    let n = state.write_message(&[], &mut payload)?;
    let frame = Frame::Response {
        data: STANDARD.encode(&payload[..n]),
    };
    ensure!(state.is_handshake_finished(), "incomplete handshake");
    Ok((
        Channel {
            state: state.into_transport_mode()?,
            pending: Vec::new(),
        },
        frame,
    ))
}

pub struct Channel {
    state: TransportState,
    pending: Vec<u8>,
}
impl Channel {
    pub fn seal(&mut self, packet: &impl Serialize) -> Result<Vec<Frame>> {
        let data = serde_json::to_vec(packet)?;
        ensure!(
            !data.is_empty() && data.len() <= MAX_FRAME,
            "application message exceeds limit"
        );
        let count = data.len().div_ceil(CHUNK);
        let mut frames = Vec::with_capacity(count);
        for (i, chunk) in data.chunks(CHUNK).enumerate() {
            // Final-fragment marker is authenticated/encrypted, not an outer routing hint.
            let mut plain = Vec::with_capacity(chunk.len() + 1);
            plain.push(u8::from(i + 1 == count));
            plain.extend_from_slice(chunk);
            let mut encrypted = vec![0; plain.len() + 16];
            let n = self.state.write_message(&plain, &mut encrypted)?;
            frames.push(Frame::Data {
                data: STANDARD.encode(&encrypted[..n]),
            });
        }
        Ok(frames)
    }
    pub fn open(&mut self, frame: Frame) -> Result<Option<Vec<u8>>> {
        let Frame::Data { data } = frame else {
            bail!("expected encrypted data");
        };
        let encrypted = decode(&data, NOISE_LIMIT)?;
        let mut plain = vec![0; encrypted.len()];
        let n = self
            .state
            .read_message(&encrypted, &mut plain)
            .context("encrypted frame authentication failed")?;
        ensure!((2..=CHUNK + 1).contains(&n), "invalid encrypted fragment");
        ensure!(
            plain[0] <= 1 && self.pending.len() + n - 1 <= MAX_FRAME,
            "invalid/oversized reassembly"
        );
        self.pending.extend_from_slice(&plain[1..n]);
        if plain[0] == 1 {
            Ok(Some(std::mem::take(&mut self.pending)))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (Channel, Channel) {
        let psk = "42".repeat(32);
        let (initiator, init) = Initiator::new(&psk, b"session/target").unwrap();
        let (responder, response) = respond(&psk, b"session/target", init).unwrap();
        (initiator.finish(response).unwrap(), responder)
    }
    fn roundtrip(a: &mut Channel, b: &mut Channel, value: &serde_json::Value) {
        let frames = a.seal(value).unwrap();
        let mut received = None;
        for frame in frames {
            let chunk = b.open(frame).unwrap();
            if chunk.is_some() {
                assert!(received.is_none());
                received = chunk;
            }
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&received.unwrap()).unwrap(),
            *value
        );
    }
    #[test]
    fn messages_and_fragmentation_are_bidirectional() {
        let (mut a, mut b) = pair();
        roundtrip(
            &mut a,
            &mut b,
            &serde_json::json!({"command":"not visible to relay"}),
        );
        roundtrip(
            &mut b,
            &mut a,
            &serde_json::json!({"output":"x".repeat(200_000)}),
        );
        roundtrip(&mut a, &mut b, &serde_json::json!([1, 2, 3]));
    }
    #[test]
    fn wrong_key_or_session_is_rejected() {
        let (_, init) = Initiator::new(&"42".repeat(32), b"one").unwrap();
        assert!(respond(&"43".repeat(32), b"one", init.clone()).is_err());
        assert!(respond(&"42".repeat(32), b"two", init).is_err());
    }
    #[test]
    fn replay_and_cross_channel_frames_are_rejected() {
        let (mut a, mut b) = pair();
        let frame = a.seal(&123).unwrap().remove(0);
        b.open(frame.clone()).unwrap();
        assert!(b.open(frame.clone()).is_err());
        let (_, mut other) = pair();
        assert!(other.open(frame).is_err());
    }
    #[test]
    fn tampering_and_reordering_are_rejected() {
        let (mut a, mut b) = pair();
        let mut frames = a.seal(&"x".repeat(120_000)).unwrap();
        assert!(b.open(frames.remove(1)).is_err());
        let (mut a, mut b) = pair();
        let Frame::Data { data } = a.seal(&123).unwrap().remove(0) else {
            unreachable!()
        };
        let mut data = STANDARD.decode(data).unwrap();
        data[0] ^= 1;
        assert!(
            b.open(Frame::Data {
                data: STANDARD.encode(data)
            })
            .is_err()
        );
    }
    #[test]
    fn size_and_message_type_limits() {
        let (mut a, mut b) = pair();
        assert!(a.seal(&"x".repeat(MAX_FRAME)).is_err());
        assert!(
            b.open(Frame::Data {
                data: "A".repeat(MAX_FRAME)
            })
            .is_err()
        );
        assert!(
            b.open(Frame::RelayEvent {
                code: "fake".into()
            })
            .is_err()
        );
    }
}
