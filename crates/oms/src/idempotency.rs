//! Deterministic idempotency keys shared with the Python bridge (`docs/RUNTIME.md` rule 4).
//!
//! `key = uuid5(namespace, intent_id + "|" + canonical_payload)` with
//! `canonical_payload = "{exchange_id}|{side}|{action}|{quantity}|{price_pips}|{expiration_unix_s}|{tournament_id}"`.
//! The same decision with the same payload always yields the same key, so a retry (or a restart)
//! can never mint a second order for it.

use uuid::Uuid;

use susq_core::{Action, ExchangeId, Pips, Qty, Side, TournamentId};

/// Everything that identifies one order payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadParts<'a> {
    pub exchange_id: &'a ExchangeId,
    pub side: Side,
    pub action: Action,
    pub quantity: Qty,
    pub price: Pips,
    pub expiration_unix_s: i64,
    pub tournament_id: &'a TournamentId,
}

/// `"{exchange_id}|{side}|{action}|{quantity}|{price_pips}|{expiration_unix_s}|{tournament_id}"`
pub fn canonical_payload(p: &PayloadParts<'_>) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        p.exchange_id,
        p.side.as_str(),
        p.action.as_str(),
        p.quantity.get(),
        p.price.get(),
        p.expiration_unix_s,
        p.tournament_id
    )
}

/// `uuid5(namespace, intent_id + "|" + canonical_payload)`
pub fn idempotency_key(namespace: &Uuid, intent_id: &str, canonical_payload: &str) -> Uuid {
    Uuid::new_v5(namespace, format!("{intent_id}|{canonical_payload}").as_bytes())
}

/// Key for an atomic multi-leg request: the bundle id and every leg payload in order, `;`-joined.
/// (Rust-only: the bridge does not send multi-leg orders.)
pub fn bundle_key(namespace: &Uuid, bundle_id: &str, leg_payloads: &[String]) -> Uuid {
    idempotency_key(namespace, bundle_id, &leg_payloads.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMESPACE: &str = "6f1c0e2a-7c55-4d7e-9a8e-5f2b3c1d0a99";

    #[test]
    fn matches_the_cross_language_vector() {
        let ns = Uuid::parse_str(NAMESPACE).expect("namespace");
        let ex = ExchangeId::new("381");
        let tid = TournamentId::new("t-uuid");
        let payload = canonical_payload(&PayloadParts {
            exchange_id: &ex,
            side: Side::No,
            action: Action::Buy,
            quantity: Qty(37),
            price: Pips::new(1450).expect("valid"),
            expiration_unix_s: 1_791_475_230,
            tournament_id: &tid,
        });
        assert_eq!(payload, "381|no|buy|37|1450|1791475230|t-uuid");
        let key = idempotency_key(&ns, "racer:rel-9:1791475200", &payload);
        assert_eq!(key.to_string(), "c5fe4b7e-7efc-55fb-82b5-1bd6356d699c");
    }

    #[test]
    fn any_payload_change_changes_the_key() {
        let ns = Uuid::parse_str(NAMESPACE).expect("namespace");
        let a = idempotency_key(&ns, "i", "1|yes|buy|1|5000|10|t");
        assert_ne!(a, idempotency_key(&ns, "i", "1|yes|buy|2|5000|10|t"));
        assert_ne!(a, idempotency_key(&ns, "j", "1|yes|buy|1|5000|10|t"));
        assert_eq!(a, idempotency_key(&ns, "i", "1|yes|buy|1|5000|10|t"));
        assert_ne!(
            bundle_key(&ns, "b", &["x".into(), "y".into()]),
            bundle_key(&ns, "b", &["y".into(), "x".into()])
        );
    }
}
