//! Decoding of inbound bus payloads. Pure functions so they can be unit tested and fuzzed.

use susq_contracts::{decode, v1};

use crate::subjects;
use crate::watchdog::NeedleStream;

/// One decoded message from a `needle.*` subject.
#[derive(Debug, Clone, PartialEq)]
pub enum NeedleMessage {
    /// `needle.race_posteriors`
    Posteriors(v1::RacePosteriors),
    /// `needle.national_shift`
    Shift(v1::NationalShift),
    /// `needle.heartbeat`
    Heartbeat(v1::Heartbeat),
    /// The payload did not decode (or arrived on an unexpected subject).
    Malformed { subject: String, error: String },
}

impl NeedleMessage {
    /// Decode `payload` according to the subject it arrived on.
    pub fn decode(subject: &str, payload: &[u8]) -> Self {
        let malformed = |error: String| Self::Malformed {
            subject: subject.to_owned(),
            error,
        };
        let decoded = match NeedleStream::from_subject(subject) {
            Some(NeedleStream::Posteriors) => decode(payload).map(Self::Posteriors),
            Some(NeedleStream::Shift) => decode(payload).map(Self::Shift),
            Some(NeedleStream::Heartbeat) => decode(payload).map(Self::Heartbeat),
            None => return malformed("not a needle subject".to_owned()),
        };
        decoded.unwrap_or_else(|e| malformed(e.to_string()))
    }

    /// The stream this message belongs to (`None` only for a malformed foreign subject).
    pub fn stream(&self) -> Option<NeedleStream> {
        match self {
            Self::Posteriors(_) => Some(NeedleStream::Posteriors),
            Self::Shift(_) => Some(NeedleStream::Shift),
            Self::Heartbeat(_) => Some(NeedleStream::Heartbeat),
            Self::Malformed { subject, .. } => NeedleStream::from_subject(subject),
        }
    }

    /// The message header, if the payload decoded and carried one.
    pub fn header(&self) -> Option<&v1::Header> {
        match self {
            Self::Posteriors(m) => m.header.as_ref(),
            Self::Shift(m) => m.header.as_ref(),
            Self::Heartbeat(m) => m.header.as_ref(),
            Self::Malformed { .. } => None,
        }
    }
}

/// Issuer stamped on a kill synthesized from an undecodable payload.
pub const UNKNOWN_KILL_ISSUER: &str = "unknown";

/// Decode an `ops.kill` payload. **Fail safe:** this never drops a kill. The header is not checked
/// (a kill with a missing or foreign-schema header still stops trading), and a payload that does
/// not decode at all still yields a [`v1::Kill`] whose reason says so. A spurious kill costs some
/// trading time; a dropped kill can cost the account.
pub fn decode_kill(payload: &[u8]) -> v1::Kill {
    decode::<v1::Kill>(payload).unwrap_or_else(|e| v1::Kill {
        header: None,
        reason: format!("malformed {} payload: {e}", subjects::OPS_KILL),
        issuer: UNKNOWN_KILL_ISSUER.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use susq_contracts::encode;

    /// Bytes that are not valid protobuf (field 1, wire type 7 is reserved).
    const GARBAGE: &[u8] = &[0x0f, 0xff, 0xff];

    #[test]
    fn decodes_each_needle_subject() {
        let hb = v1::Heartbeat {
            status: "ok".to_owned(),
            ..Default::default()
        };
        let msg = NeedleMessage::decode(subjects::NEEDLE_HEARTBEAT, &encode(&hb));
        assert_eq!(msg, NeedleMessage::Heartbeat(hb));
        assert_eq!(msg.stream(), Some(NeedleStream::Heartbeat));
        assert_eq!(msg.header(), None);

        let shift = v1::NationalShift {
            header: Some(v1::Header {
                seq: 4,
                ..Default::default()
            }),
            mean: 1.0,
            ..Default::default()
        };
        let msg = NeedleMessage::decode(subjects::NEEDLE_NATIONAL_SHIFT, &encode(&shift));
        assert_eq!(msg.header().map(|h| h.seq), Some(4));
        assert_eq!(msg, NeedleMessage::Shift(shift));

        let post = v1::RacePosteriors::default();
        let msg = NeedleMessage::decode(subjects::NEEDLE_RACE_POSTERIORS, &encode(&post));
        assert_eq!(msg, NeedleMessage::Posteriors(post));
    }

    #[test]
    fn garbage_is_malformed_but_keeps_its_stream() {
        let msg = NeedleMessage::decode(subjects::NEEDLE_RACE_POSTERIORS, GARBAGE);
        assert!(matches!(&msg, NeedleMessage::Malformed { subject, .. }
            if subject == subjects::NEEDLE_RACE_POSTERIORS));
        assert_eq!(msg.stream(), Some(NeedleStream::Posteriors));
    }

    #[test]
    fn foreign_subject_is_malformed() {
        let msg = NeedleMessage::decode("needle.other", &[]);
        assert!(matches!(msg, NeedleMessage::Malformed { .. }));
        assert_eq!(msg.stream(), None);
    }

    #[test]
    fn kill_round_trips() {
        let kill = v1::Kill {
            header: None,
            reason: "manual".to_owned(),
            issuer: "human".to_owned(),
        };
        assert_eq!(decode_kill(&encode(&kill)), kill);
    }

    #[test]
    fn malformed_kill_is_still_a_kill() {
        let kill = decode_kill(GARBAGE);
        assert!(kill.reason.starts_with("malformed ops.kill payload"));
        assert_eq!(kill.issuer, UNKNOWN_KILL_ISSUER);
        // An empty payload is a valid (default) Kill and is honored too.
        assert_eq!(decode_kill(&[]), v1::Kill::default());
    }
}
