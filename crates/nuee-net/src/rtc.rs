//! The optional direct lane between a browser worker and a browser coordinator, over a WebRTC
//! data channel, and the checks applied to its signalling.
//!
//! Signalling (`Msg::RtcOffer/RtcAnswer/RtcIce`) always travels through iroh, relayed by the
//! coordinator when the presenter turned WebRTC on. Once the channel is open:
//!
//! - worker → coordinator: only `Msg::Result` frames take the lane (`TAG_RESULT` followed by
//!   the postcard bytes of the message). Everything else stays on iroh.
//! - coordinator → worker: one `TAG_ACK` frame per result received (the lease id, `u64` LE).
//!
//! The worker keeps every result it sent on the lane until it is acknowledged. When the
//! channel closes or fails, the unacknowledged results are sent again through iroh. A result
//! that arrives twice (sent on the lane, delivered, but its ack lost) is harmless: the
//! coordinator settles a lease once and drops any later result for it.

use std::collections::BTreeMap;

use nuee_core::RtcKind;
use nuee_proto::Msg;
use nuee_proto::limits::MAX_SIGNAL;

/// Frame tag: a `Msg::Result` (worker → coordinator).
pub const TAG_RESULT: u8 = 0;
/// Frame tag: acknowledgement of a lease's result (coordinator → worker).
pub const TAG_ACK: u8 = 1;
/// Largest frame sent on the lane; bigger results go through iroh. Browsers agree on data
/// channel messages up to 256 KiB.
pub const MAX_LANE_FRAME: usize = 240 * 1024;
/// Results waiting for an ack before new ones fall back to iroh.
pub const MAX_UNACKED: usize = 64;

/// A signalling payload was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignalError {
    /// Longer than `MAX_SIGNAL`.
    #[error("signalling payload of {0} bytes exceeds the cap")]
    TooLarge(usize),
    /// Contains a control character other than CR, LF or tab.
    #[error("signalling payload contains control characters")]
    Control,
    /// Not shaped like an SDP description (offer, answer) or an ICE candidate in JSON.
    #[error("signalling payload is not a {0}")]
    Shape(&'static str),
}

/// Checks a signalling payload before it is relayed or handed to the page.
pub fn check_signal(kind: RtcKind, payload: &str) -> Result<(), SignalError> {
    if payload.len() > MAX_SIGNAL {
        return Err(SignalError::TooLarge(payload.len()));
    }
    if payload
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(SignalError::Control);
    }
    match kind {
        RtcKind::Offer | RtcKind::Answer => {
            if !payload.starts_with("v=0") {
                return Err(SignalError::Shape("session description"));
            }
        }
        RtcKind::Ice => {
            let t = payload.trim();
            if !(t.is_empty() || (t.starts_with('{') && t.ends_with('}'))) {
                return Err(SignalError::Shape("candidate"));
            }
        }
    }
    Ok(())
}

/// A lane frame was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaneError {
    /// Empty frame.
    #[error("empty lane frame")]
    Empty,
    /// Above `MAX_LANE_FRAME`.
    #[error("lane frame of {0} bytes exceeds the cap")]
    TooLarge(usize),
    /// Unknown first byte, or a tag not expected in this direction.
    #[error("unexpected lane frame tag {0}")]
    Tag(u8),
    /// An ack that is not exactly 8 bytes of lease id.
    #[error("malformed lane ack")]
    Ack,
    /// The message does not decode.
    #[error("undecodable lane message: {0}")]
    Decode(String),
    /// A decoded message other than `Msg::Result`.
    #[error("only results travel on the lane")]
    NotResult,
}

/// Decodes a worker → coordinator frame: the lease id and the `Msg::Result` to hand to the
/// coordinator exactly as if it came from the peer's iroh stream.
pub fn decode_result(frame: &[u8]) -> Result<(u64, Msg), LaneError> {
    if frame.len() > MAX_LANE_FRAME {
        return Err(LaneError::TooLarge(frame.len()));
    }
    let (&tag, body) = frame.split_first().ok_or(LaneError::Empty)?;
    if tag != TAG_RESULT {
        return Err(LaneError::Tag(tag));
    }
    let msg: Msg = nuee_proto::decode(body).map_err(|e| LaneError::Decode(e.to_string()))?;
    match msg {
        Msg::Result { lease, .. } => Ok((lease, msg)),
        _ => Err(LaneError::NotResult),
    }
}

/// The coordinator's answer to a result received on the lane.
pub fn ack_frame(lease: u64) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = TAG_ACK;
    out[1..].copy_from_slice(&lease.to_le_bytes());
    out
}

/// Decodes a coordinator → worker frame into the acknowledged lease id.
pub fn decode_ack(frame: &[u8]) -> Result<u64, LaneError> {
    let (&tag, body) = frame.split_first().ok_or(LaneError::Empty)?;
    if tag != TAG_ACK {
        return Err(LaneError::Tag(tag));
    }
    let bytes: [u8; 8] = body.try_into().map_err(|_| LaneError::Ack)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Where an outbound message goes.
#[derive(Debug)]
pub enum Route {
    /// This frame, on the data channel.
    Lane(Vec<u8>),
    /// The message, on the iroh stream.
    Iroh(Msg),
}

/// The worker side of the lane: routes outbound messages and remembers the results that
/// still need an ack.
#[derive(Debug, Default)]
pub struct Lane {
    open: bool,
    unacked: BTreeMap<u64, Msg>,
}

impl Lane {
    /// A closed lane: everything goes through iroh.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the data channel is open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Results sent on the lane and not acknowledged yet.
    pub fn unacked(&self) -> usize {
        self.unacked.len()
    }

    /// The data channel opened.
    pub fn open(&mut self) {
        self.open = true;
    }

    /// Picks the path of `msg`: results take the open lane (while few enough wait for an ack
    /// and the frame fits), everything else iroh.
    pub fn route(&mut self, msg: Msg) -> Route {
        let lease = match &msg {
            Msg::Result { lease, .. } if self.open && self.unacked.len() < MAX_UNACKED => *lease,
            _ => return Route::Iroh(msg),
        };
        let body = match nuee_proto::encode(&msg) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(lease, "result not encodable for the lane: {e}");
                return Route::Iroh(msg);
            }
        };
        if body.len() + 1 > MAX_LANE_FRAME {
            return Route::Iroh(msg);
        }
        let mut frame = Vec::with_capacity(body.len() + 1);
        frame.push(TAG_RESULT);
        frame.extend_from_slice(&body);
        self.unacked.insert(lease, msg);
        Route::Lane(frame)
    }

    /// An ack arrived; returns the lease it settles (`None` when it was not waiting).
    pub fn ack(&mut self, frame: &[u8]) -> Result<Option<u64>, LaneError> {
        let lease = decode_ack(frame)?;
        Ok(self.unacked.remove(&lease).map(|_| lease))
    }

    /// The data channel closed or failed: returns the results to resend through iroh, in
    /// lease order. Later results go through iroh until the lane opens again.
    pub fn close(&mut self) -> Vec<Msg> {
        self.open = false;
        std::mem::take(&mut self.unacked).into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuee_proto::{Hash, Payload, Signature};

    fn result(lease: u64, len: usize) -> Msg {
        let bytes = vec![7u8; len];
        Msg::Result {
            lease,
            output_hash: Hash::of(&bytes),
            output: Payload::Inline(bytes.try_into().expect("inline")),
            duration_ms: 3,
            sig: Signature::from_bytes(&[0u8; 64]),
        }
    }

    #[test]
    fn closed_lane_sends_everything_through_iroh() {
        let mut lane = Lane::new();
        assert!(matches!(lane.route(result(1, 8)), Route::Iroh(_)));
        assert_eq!(lane.unacked(), 0);
    }

    #[test]
    fn results_take_the_open_lane_and_others_stay_on_iroh() {
        let mut lane = Lane::new();
        lane.open();
        assert!(matches!(lane.route(Msg::Want { slots: 2 }), Route::Iroh(_)));
        let Route::Lane(frame) = lane.route(result(5, 100)) else {
            panic!("result should take the lane");
        };
        let (lease, msg) = decode_result(&frame).expect("frame decodes");
        assert_eq!(lease, 5);
        assert!(matches!(msg, Msg::Result { lease: 5, .. }));
        assert_eq!(lane.unacked(), 1);
    }

    #[test]
    fn ack_settles_and_close_resends_only_the_rest() {
        let mut lane = Lane::new();
        lane.open();
        for l in [3, 1, 2] {
            assert!(matches!(lane.route(result(l, 16)), Route::Lane(_)));
        }
        assert_eq!(lane.ack(&ack_frame(2)), Ok(Some(2)));
        assert_eq!(lane.ack(&ack_frame(2)), Ok(None));
        assert_eq!(lane.ack(&ack_frame(9)), Ok(None));
        let resend: Vec<u64> = lane
            .close()
            .into_iter()
            .map(|m| match m {
                Msg::Result { lease, .. } => lease,
                _ => u64::MAX,
            })
            .collect();
        assert_eq!(resend, vec![1, 3]);
        assert!(!lane.is_open());
        assert!(matches!(lane.route(result(4, 16)), Route::Iroh(_)));
    }

    #[test]
    fn large_results_and_a_full_window_fall_back() {
        let mut lane = Lane::new();
        lane.open();
        assert!(matches!(lane.route(result(1, 300 * 1024)), Route::Iroh(_)));
        for l in 0..MAX_UNACKED as u64 {
            assert!(matches!(lane.route(result(10 + l, 4)), Route::Lane(_)));
        }
        assert!(matches!(lane.route(result(999, 4)), Route::Iroh(_)));
    }

    #[test]
    fn coordinator_refuses_anything_but_results() {
        assert!(matches!(decode_result(&[]), Err(LaneError::Empty)));
        let want = nuee_proto::encode(&Msg::Want { slots: 1 }).expect("encode");
        let mut frame = vec![TAG_RESULT];
        frame.extend_from_slice(&want);
        assert!(matches!(decode_result(&frame), Err(LaneError::NotResult)));
        assert!(matches!(
            decode_result(&[TAG_ACK, 0]),
            Err(LaneError::Tag(TAG_ACK))
        ));
        assert!(matches!(
            decode_result(&[TAG_RESULT, 0xff, 0xff]),
            Err(LaneError::Decode(_))
        ));
        assert!(matches!(
            decode_result(&vec![TAG_RESULT; MAX_LANE_FRAME + 1]),
            Err(LaneError::TooLarge(_))
        ));
        assert_eq!(decode_ack(&[TAG_ACK, 1, 2]), Err(LaneError::Ack));
        assert_eq!(decode_ack(&ack_frame(u64::MAX)), Ok(u64::MAX));
    }

    #[test]
    fn signals_are_checked() {
        let sdp = "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\n";
        assert_eq!(check_signal(RtcKind::Offer, sdp), Ok(()));
        assert_eq!(check_signal(RtcKind::Answer, sdp), Ok(()));
        assert_eq!(
            check_signal(
                RtcKind::Ice,
                r#"{"candidate":"candidate:1 1 udp 1 127.0.0.1 5000 typ host"}"#
            ),
            Ok(())
        );
        assert_eq!(check_signal(RtcKind::Ice, ""), Ok(()));
        assert!(matches!(
            check_signal(RtcKind::Offer, "hello"),
            Err(SignalError::Shape(_))
        ));
        assert!(matches!(
            check_signal(RtcKind::Ice, "v=0"),
            Err(SignalError::Shape(_))
        ));
        assert_eq!(
            check_signal(RtcKind::Offer, "v=0\u{0}"),
            Err(SignalError::Control)
        );
        let big = format!("v=0{}", "a".repeat(MAX_SIGNAL));
        assert!(matches!(
            check_signal(RtcKind::Offer, &big),
            Err(SignalError::TooLarge(_))
        ));
    }
}
