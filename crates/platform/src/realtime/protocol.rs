//! Phoenix channel protocol (serializer `vsn=1.0.0`, JSON objects) as spoken by Supabase Realtime.
//!
//! Every frame is `{"topic","event","payload","ref","join_ref"?}`. Channel topics are
//! `realtime:{channel}`; heartbeats go to the `phoenix` topic. Frames carrying the access token
//! have a redacting `Debug` so they can never leak through logs.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use super::lenient;
use crate::error::PlatformError;

/// Prefix Supabase Realtime puts in front of every channel name.
pub const TOPIC_PREFIX: &str = "realtime:";
/// Phoenix protocol serializer version requested in the websocket URL.
pub const PROTOCOL_VSN: &str = "1.0.0";
/// Topic that receives heartbeats.
const PHOENIX_TOPIC: &str = "phoenix";

/// Build `{wss|ws}://host/realtime/v1/websocket?apikey=..&vsn=1.0.0` from a `supabaseUrl`.
///
/// The error message never contains the key or the input URL.
pub fn websocket_url(supabase_url: &str, anon_key: &str) -> Result<Url, PlatformError> {
    let mut url = Url::parse(supabase_url.trim())
        .map_err(|e| PlatformError::Invalid(format!("realtime supabaseUrl is not a URL: {e}")))?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        other => {
            return Err(PlatformError::Invalid(format!(
                "realtime supabaseUrl has unsupported scheme {other:?}"
            )));
        }
    };
    url.set_scheme(scheme)
        .map_err(|()| PlatformError::Invalid("realtime supabaseUrl scheme rejected".into()))?;
    let path = format!("{}/realtime/v1/websocket", url.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_fragment(None);
    url.query_pairs_mut()
        .clear()
        .append_pair("apikey", anon_key)
        .append_pair("vsn", PROTOCOL_VSN);
    Ok(url)
}

/// Topic for a channel name (`tournament:t1` -> `realtime:tournament:t1`).
pub fn topic(channel: &str) -> String {
    format!("{TOPIC_PREFIX}{channel}")
}

/// One Phoenix message.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Frame {
    pub topic: String,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(rename = "ref", default, deserialize_with = "lenient::opt_string")]
    pub reference: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient::opt_string"
    )]
    pub join_ref: Option<String>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("topic", &self.topic)
            .field("event", &self.event)
            .field("ref", &self.reference)
            .field("join_ref", &self.join_ref)
            .finish_non_exhaustive()
    }
}

impl Frame {
    /// `phx_join` for a private broadcast channel, authorized with `access_token`.
    pub fn join(channel: &str, access_token: &str, reference: &str) -> Self {
        Self {
            topic: topic(channel),
            event: "phx_join".into(),
            payload: json!({
                "config": {
                    "broadcast": {"ack": false, "self": false},
                    "presence": {"key": ""},
                    "private": true
                },
                "access_token": access_token
            }),
            reference: Some(reference.to_owned()),
            join_ref: Some(reference.to_owned()),
        }
    }

    /// Heartbeat on the `phoenix` topic.
    pub fn heartbeat(reference: &str) -> Self {
        Self {
            topic: PHOENIX_TOPIC.into(),
            event: "heartbeat".into(),
            payload: json!({}),
            reference: Some(reference.to_owned()),
            join_ref: None,
        }
    }

    /// Push a refreshed access token to a joined channel.
    pub fn access_token(channel: &str, access_token: &str, join_ref: &str, reference: &str) -> Self {
        Self {
            topic: topic(channel),
            event: "access_token".into(),
            payload: json!({ "access_token": access_token }),
            reference: Some(reference.to_owned()),
            join_ref: Some(join_ref.to_owned()),
        }
    }

    /// JSON text of the frame.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse a text frame.
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Classify an inbound frame.
    pub fn classify(self) -> Inbound {
        if self.topic == PHOENIX_TOPIC {
            return match self.event.as_str() {
                "phx_reply" => Inbound::HeartbeatReply {
                    reference: self.reference,
                },
                _ => Inbound::Other,
            };
        }
        let Some(channel) = self.topic.strip_prefix(TOPIC_PREFIX).map(str::to_owned) else {
            return Inbound::Other;
        };
        match self.event.as_str() {
            "phx_reply" => Inbound::Reply {
                channel,
                reference: self.reference,
                ok: self.payload.get("status").and_then(Value::as_str) == Some("ok"),
                reason: self
                    .payload
                    .pointer("/response/reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            "phx_error" | "phx_close" => Inbound::ChannelDown {
                channel,
                reference: self.join_ref.or(self.reference),
                event: self.event,
            },
            "broadcast" => {
                let mut payload = self.payload;
                let event = payload
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Inbound::Broadcast {
                    channel,
                    event,
                    payload: payload.get_mut("payload").map(Value::take).unwrap_or(Value::Null),
                }
            }
            "system" => Inbound::System {
                channel,
                ok: self.payload.get("status").and_then(Value::as_str) != Some("error"),
                message: self
                    .payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            },
            _ => Inbound::Other,
        }
    }
}

/// What an inbound frame means to the client.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Inbound {
    /// Reply to one of our heartbeats.
    HeartbeatReply { reference: Option<String> },
    /// Reply to a push on a channel (joins are the ones we track); `reason` explains a rejection.
    Reply {
        channel: String,
        reference: Option<String>,
        ok: bool,
        reason: Option<String>,
    },
    /// The server errored or closed the channel; `reference` is its join ref when sent.
    ChannelDown {
        channel: String,
        reference: Option<String>,
        event: String,
    },
    /// A server broadcast with its inner event name and inner payload.
    Broadcast {
        channel: String,
        event: String,
        payload: Value,
    },
    /// Supabase `system` status message for a channel.
    System {
        channel: String,
        ok: bool,
        message: String,
    },
    /// Presence and anything else this client ignores.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_maps_scheme_and_appends_query() {
        let u = websocket_url("https://abc.supabase.co", "anon-key").expect("url");
        assert_eq!(
            u.as_str(),
            "wss://abc.supabase.co/realtime/v1/websocket?apikey=anon-key&vsn=1.0.0"
        );
        let u = websocket_url("http://127.0.0.1:5432/", "k").expect("url");
        assert_eq!(
            u.as_str(),
            "ws://127.0.0.1:5432/realtime/v1/websocket?apikey=k&vsn=1.0.0"
        );
    }

    #[test]
    fn url_keeps_base_path_and_escapes_key() {
        let u = websocket_url("https://proxy.example/sb/", "a+b&c").expect("url");
        assert_eq!(
            u.as_str(),
            "wss://proxy.example/sb/realtime/v1/websocket?apikey=a%2Bb%26c&vsn=1.0.0"
        );
    }

    #[test]
    fn url_errors_never_echo_the_key() {
        for bad in ["not a url", "ftp://x.example"] {
            let e = websocket_url(bad, "sekrit").expect_err("rejected");
            assert!(!e.to_string().contains("sekrit"));
        }
    }

    #[test]
    fn join_frame_matches_supabase_private_channel_join() {
        let f = Frame::join("tournament:t1", "jwt", "3");
        let v: Value = serde_json::from_str(&f.encode().expect("encode")).expect("json");
        assert_eq!(
            v,
            json!({
                "topic": "realtime:tournament:t1",
                "event": "phx_join",
                "payload": {
                    "config": {"broadcast": {"ack": false, "self": false},
                               "presence": {"key": ""}, "private": true},
                    "access_token": "jwt"
                },
                "ref": "3",
                "join_ref": "3"
            })
        );
    }

    #[test]
    fn heartbeat_and_access_token_frames() {
        let v: Value = serde_json::from_str(&Frame::heartbeat("9").encode().expect("encode")).expect("json");
        assert_eq!(
            v,
            json!({"topic": "phoenix", "event": "heartbeat", "payload": {}, "ref": "9"})
        );
        let f = Frame::access_token("user:p", "new", "3", "10");
        assert_eq!(f.topic, "realtime:user:p");
        assert_eq!(f.payload, json!({"access_token": "new"}));
        assert_eq!(f.join_ref.as_deref(), Some("3"));
    }

    #[test]
    fn debug_never_prints_the_token() {
        let f = Frame::join("user:p", "jwt-secret", "1");
        assert!(!format!("{f:?}").contains("jwt-secret"));
    }

    #[test]
    fn classify_replies_and_downs() {
        let f = Frame::decode(
            r#"{"topic":"realtime:user:p","event":"phx_reply","payload":{"status":"ok","response":{}},"ref":1}"#,
        )
        .expect("decode");
        assert_eq!(
            f.classify(),
            Inbound::Reply {
                channel: "user:p".into(),
                reference: Some("1".into()),
                ok: true,
                reason: None
            }
        );
        let f =
            Frame::decode(r#"{"topic":"phoenix","event":"phx_reply","payload":{"status":"ok"},"ref":"4"}"#)
                .expect("decode");
        assert_eq!(
            f.classify(),
            Inbound::HeartbeatReply {
                reference: Some("4".into())
            }
        );
        let f = Frame::decode(
            r#"{"topic":"realtime:user:p","event":"phx_close","payload":{},"ref":null,"join_ref":"2"}"#,
        )
        .expect("decode");
        assert_eq!(
            f.classify(),
            Inbound::ChannelDown {
                channel: "user:p".into(),
                reference: Some("2".into()),
                event: "phx_close".into()
            }
        );
    }

    #[test]
    fn classify_rejected_join_carries_reason() {
        let f = Frame::decode(
            r#"{"topic":"realtime:user:p","event":"phx_reply","ref":"5",
                "payload":{"status":"error","response":{"reason":"Unauthorized"}}}"#,
        )
        .expect("decode");
        assert!(matches!(
            f.classify(),
            Inbound::Reply { ok: false, reason: Some(r), .. } if r == "Unauthorized"
        ));
    }

    #[test]
    fn classify_broadcast_unwraps_inner_event() {
        let f = Frame::decode(
            r#"{"topic":"realtime:leaderboard:all","event":"broadcast","ref":null,
                "payload":{"type":"broadcast","event":"tick","payload":{"at":"x"}}}"#,
        )
        .expect("decode");
        assert_eq!(
            f.classify(),
            Inbound::Broadcast {
                channel: "leaderboard:all".into(),
                event: "tick".into(),
                payload: json!({"at": "x"})
            }
        );
    }

    #[test]
    fn classify_ignores_foreign_topics_and_presence() {
        let f = Frame::decode(r#"{"topic":"other","event":"broadcast","payload":{}}"#).expect("decode");
        assert_eq!(f.classify(), Inbound::Other);
        let f =
            Frame::decode(r#"{"topic":"realtime:x","event":"presence_state","payload":{}}"#).expect("decode");
        assert_eq!(f.classify(), Inbound::Other);
    }
}
