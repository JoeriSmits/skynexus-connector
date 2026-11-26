//! The TechLog snapshot, schema version 1: what the Connector POSTs to the
//! EFB server.
//!
//! Field names are camelCase, every key is always present and nulls are
//! written, because the server validates it with a strict schema.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::model::{DispatchStatus, State};

/// The snapshot schema version.
pub const SNAPSHOT_VERSION: u8 = 1;

/// The most items one snapshot carries.
pub const MAX_ITEMS: usize = 60;

/// The categories the server accepts; anything else becomes null.
const CATEGORIES: [&str; 5] = ["A", "B", "C", "D", "NO-GO"];

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub v: u8,
    /// SHA-256 hex of the snapshot without `hash` and `readAt`.
    pub hash: String,
    pub techlog_version: String,
    /// ISO 8601 UTC, such as `2026-11-26T10:00:00.000Z`.
    pub read_at: String,
    /// Whether the TechLog airframe is the aircraft loaded in the sim.
    pub loaded: bool,
    pub sim_registration: Option<String>,
    pub airframe: Airframe,
    pub dispatch: DispatchStatus,
    pub reasons: Vec<String>,
    pub items: Vec<Item>,
    pub fuel_factor_pct: Option<f64>,
    pub sim_brief_factor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Airframe {
    pub registration: String,
    pub family: String,
    pub family_label: Option<String>,
    pub variant: Option<String>,
    pub engine: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: i64,
    pub mel_id: Option<String>,
    pub ata: Option<i64>,
    pub title: String,
    pub category: Option<String>,
    pub status: String,
    pub raised_utc: Option<String>,
    pub deferred_until: Option<String>,
    pub placard: Option<String>,
    pub restrictions: Option<String>,
    pub ops: Option<String>,
    pub position: Option<String>,
    pub label: Option<String>,
    pub performance_note: Option<String>,
}

/// Builds the snapshot from TechLog's state. None when TechLog has no airframe.
pub fn build(state: &State, techlog_version: &str, read_at: DateTime<Utc>) -> Option<Snapshot> {
    let af = state.airframe.as_ref()?;
    let loaded = state
        .sim
        .registration
        .as_deref()
        .map(normalise_registration)
        .is_some_and(|sim| !sim.is_empty() && sim == normalise_registration(&af.registration));
    let items = state
        .defects
        .iter()
        .filter(|d| d.rectified_utc.is_none())
        .take(MAX_ITEMS)
        .map(|d| Item {
            id: d.id,
            mel_id: d.mel_id.clone(),
            ata: d.ata,
            title: d.title.clone(),
            category: d.category.clone().filter(|c| CATEGORIES.contains(&c.as_str())),
            status: d.status.clone(),
            raised_utc: d.raised_utc.clone(),
            deferred_until: d.deferred_until.clone(),
            placard: d.placard.clone(),
            restrictions: d.restrictions.clone(),
            ops: d.ops.clone(),
            position: d.position.clone(),
            label: d.label.clone(),
            performance_note: d.performance_note.clone(),
        })
        .collect();
    let perf = af.performance.as_ref();
    let mut snap = Snapshot {
        v: SNAPSHOT_VERSION,
        hash: String::new(),
        techlog_version: techlog_version.to_string(),
        read_at: read_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        loaded,
        sim_registration: state.sim.registration.clone(),
        airframe: Airframe {
            registration: af.registration.clone(),
            family: af.family.clone(),
            family_label: af.family_label.clone(),
            variant: af.variant.clone(),
            engine: af.engine.clone(),
        },
        dispatch: state.dispatch.status,
        reasons: state.dispatch.reasons.clone(),
        items,
        fuel_factor_pct: perf.and_then(|p| p.fuel_factor_pct),
        sim_brief_factor: perf.and_then(|p| p.sim_brief.clone()),
    };
    snap.hash = hash(&snap);
    Some(snap)
}

/// SHA-256 hex over the snapshot without `hash` and `readAt`, so a re-read of
/// unchanged TechLog state gives the same hash.
pub fn hash(s: &Snapshot) -> String {
    let mut value = serde_json::to_value(s).expect("snapshot serialises");
    let fields = value.as_object_mut().expect("snapshot is an object");
    fields.remove("hash");
    fields.remove("readAt");
    let json = serde_json::to_vec(&value).expect("snapshot serialises");
    hex::encode(Sha256::digest(&json))
}

/// The first 12 characters of a hash: the frame's `tl` field.
pub fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

/// Uppercase, without `-`, spaces and `.`, so `ph-eza` matches `PH-EZA`.
fn normalise_registration(reg: &str) -> String {
    reg.chars()
        .filter(|c| !matches!(c, '-' | ' ' | '.'))
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::techlog::model::{parse_state, tests::fixture};
    use chrono::TimeZone;

    fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 26, 10, min, 0).unwrap()
    }

    fn go_mel() -> State {
        parse_state(&fixture("state-fenix-go-mel.json")).unwrap()
    }

    #[test]
    fn go_mel_fixture_builds_a_loaded_snapshot() {
        let s = build(&go_mel(), "2.1.2", at(0)).unwrap();
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.dispatch, DispatchStatus::GoMel);
        assert_eq!(s.fuel_factor_pct, Some(2.3));
        assert_eq!(s.sim_brief_factor.as_deref(), Some("P02"));
        assert!(s.loaded);
        assert_eq!(s.read_at, "2026-11-26T10:00:00.000Z");
        assert_eq!(s.items[0].mel_id.as_deref(), Some("78-30-01"));
    }

    #[test]
    fn other_aircraft_in_the_sim_is_not_loaded() {
        let mut state = go_mel();
        state.sim.registration = Some("PH-BXA".into());
        assert!(!build(&state, "2.1.2", at(0)).unwrap().loaded);
    }

    #[test]
    fn no_sim_aircraft_is_not_loaded() {
        let mut state = go_mel();
        state.sim.registration = None;
        let s = build(&state, "2.1.2", at(0)).unwrap();
        assert!(!s.loaded);
        assert_eq!(s.sim_registration, None);
    }

    #[test]
    fn registration_match_ignores_case_dashes_spaces_and_dots() {
        let mut state = go_mel();
        state.sim.registration = Some("ph eza.".into());
        assert!(build(&state, "2.1.2", at(0)).unwrap().loaded);
    }

    #[test]
    fn no_airframe_gives_no_snapshot() {
        let state = parse_state(&fixture("state-no-airframe.json")).unwrap();
        assert!(build(&state, "2.1.2", at(0)).is_none());
    }

    #[test]
    fn hash_ignores_read_at() {
        let a = build(&go_mel(), "2.1.2", at(0)).unwrap();
        let b = build(&go_mel(), "2.1.2", at(7)).unwrap();
        assert_ne!(a.read_at, b.read_at);
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.hash.len(), 64);
        assert!(a.hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_eq!(hash(&a), a.hash);
    }

    #[test]
    fn hash_ignores_only_hash_and_read_at() {
        let a = build(&go_mel(), "2.1.2", at(0)).unwrap();
        let mut b = a.clone();
        b.hash = "x".into();
        b.read_at = "y".into();
        assert_eq!(hash(&b), a.hash);
        let mut c = a.clone();
        c.loaded = !c.loaded;
        assert_ne!(hash(&c), a.hash);
        let mut d = a.clone();
        d.sim_brief_factor = None;
        assert_ne!(hash(&d), a.hash);
        let mut e = a.clone();
        e.techlog_version = "2.1.3".into();
        assert_ne!(hash(&e), a.hash);
    }

    #[test]
    fn hash_changes_with_the_items() {
        let a = build(&go_mel(), "2.1.2", at(0)).unwrap();
        let mut state = go_mel();
        state.defects.pop();
        let b = build(&state, "2.1.2", at(0)).unwrap();
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn short_is_the_first_12_chars() {
        let s = build(&go_mel(), "2.1.2", at(0)).unwrap();
        assert_eq!(short(&s.hash), s.hash[..12]);
    }

    #[test]
    fn rectified_items_are_left_out_and_items_are_capped() {
        let mut state = go_mel();
        state.defects[0].rectified_utc = Some("2026-11-26T09:00:00+00:00".into());
        let s = build(&state, "2.1.2", at(0)).unwrap();
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].id, 497);

        let one = state.defects[1].clone();
        state.defects = vec![one; 75];
        let s = build(&state, "2.1.2", at(0)).unwrap();
        assert_eq!(s.items.len(), MAX_ITEMS);
    }

    #[test]
    fn unknown_category_becomes_null() {
        let mut state = go_mel();
        state.defects[0].category = Some("E".into());
        state.defects[1].category = Some("NO-GO".into());
        let s = build(&state, "2.1.2", at(0)).unwrap();
        assert_eq!(s.items[0].category, None);
        assert_eq!(s.items[1].category.as_deref(), Some("NO-GO"));
    }

    #[test]
    fn wire_keys_match_the_server_schema() {
        let mut state = go_mel();
        state.sim.registration = None;
        let s = build(&state, "2.1.2", at(0)).unwrap();
        let v = serde_json::to_value(&s).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let mut want = vec![
            "v",
            "hash",
            "techlogVersion",
            "readAt",
            "loaded",
            "simRegistration",
            "airframe",
            "dispatch",
            "reasons",
            "items",
            "fuelFactorPct",
            "simBriefFactor",
        ];
        let mut got = keys.clone();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
        assert_eq!(v["v"], 1);
        assert_eq!(v["dispatch"], "GO_MEL");
        assert!(v["simRegistration"].is_null());

        let af: Vec<&String> = v["airframe"].as_object().unwrap().keys().collect();
        assert_eq!(af.len(), 5);
        assert_eq!(v["airframe"]["familyLabel"], "Fenix A320");

        let item = v["items"][1].as_object().unwrap();
        let mut item_keys: Vec<&str> = item.keys().map(String::as_str).collect();
        item_keys.sort_unstable();
        let mut want_item = vec![
            "id",
            "melId",
            "ata",
            "title",
            "category",
            "status",
            "raisedUtc",
            "deferredUntil",
            "placard",
            "restrictions",
            "ops",
            "position",
            "label",
            "performanceNote",
        ];
        want_item.sort_unstable();
        assert_eq!(item_keys, want_item);
        assert!(item["position"].is_null());
    }
}
