//! The parts of TechLog's local HTTP API that we read.
//!
//! TechLog writes camelCase keys and always writes nulls. Only the fields the
//! snapshot needs are modelled; every other key is ignored, so TechLog can add
//! fields without breaking us. Any TechLog double may be `null` (NaN and
//! Infinity are written as `null`), so doubles are `Option<f64>`.
//!
//! Because TechLog writes every key, a nullable field in `State` is required
//! but may be null (`nullable`): a missing or renamed key is a schema error,
//! not a silent null. `Ping` is the exception, its keys really are optional.

use serde::{Deserialize, Serialize};

/// `GET /api/ping`. With the engine up: `{"ok":true,"app":"TechLog","version":"2.1.2"}`.
/// With TechLog.exe up but its engine still starting: `{"ok":true,"engine":false}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Ping {
    pub ok: bool,
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub engine: Option<bool>,
}

/// `GET /api/state`, reduced to what the snapshot uses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub sim: Sim,
    /// Null when TechLog has no airframe. Not null does not mean the aircraft
    /// is in the sim: TechLog restores the last airframe at startup.
    #[serde(deserialize_with = "nullable")]
    pub airframe: Option<Airframe>,
    pub dispatch: Dispatch,
    /// Open and deferred defects only.
    pub defects: Vec<Defect>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sim {
    /// SimConnect is connected.
    pub connected: bool,
    /// Family id of the aircraft identified in the sim; null until identified.
    #[serde(deserialize_with = "nullable")]
    pub family: Option<String>,
    /// Registration of the aircraft identified in the sim; null when none.
    #[serde(deserialize_with = "nullable")]
    pub registration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Airframe {
    pub registration: String,
    pub family: String,
    #[serde(deserialize_with = "nullable")]
    pub family_label: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub variant: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub engine: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub performance: Option<Perf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Perf {
    /// Total fuel burn penalty in percent, one decimal.
    #[serde(deserialize_with = "nullable")]
    pub fuel_factor_pct: Option<f64>,
    /// SimBrief perf code such as `"P02"` or `"M01"`.
    #[serde(deserialize_with = "nullable")]
    pub sim_brief: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispatch {
    pub status: DispatchStatus,
    pub reasons: Vec<String>,
}

/// TechLog's dispatch status. Any other string is a schema error at
/// `dispatch.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum DispatchStatus {
    #[serde(rename = "GO")]
    Go,
    #[serde(rename = "GO_MEL")]
    GoMel,
    #[serde(rename = "NO_GO")]
    NoGo,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Defect {
    pub id: i64,
    #[serde(deserialize_with = "nullable")]
    pub mel_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub ata: Option<i64>,
    pub title: String,
    /// `"A"`..`"D"` or `"NO-GO"`; anything else is dropped to null in the snapshot.
    #[serde(deserialize_with = "nullable")]
    pub category: Option<String>,
    /// PascalCase: `"Open"`, `"Deferred"`.
    pub status: String,
    #[serde(deserialize_with = "nullable")]
    pub raised_utc: Option<String>,
    /// Local date `yyyy-MM-dd`.
    #[serde(deserialize_with = "nullable")]
    pub deferred_until: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub placard: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub restrictions: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub ops: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub position: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub label: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub performance_note: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub rectified_utc: Option<String>,
}

/// A key that must be present but may be null. serde reads a missing
/// `Option` field as None; with `deserialize_with` it reports the missing field.
fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(d)
}

/// TechLog's JSON did not have the shape we expect. `path` is the JSON path
/// of the first problem, such as `dispatch` or `defects[].title` (array indices are dropped so the
/// same problem on any item reports one path).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("TechLog state does not match the expected schema at {path}")]
pub struct SchemaError {
    pub path: String,
}

/// Parses a `/api/state` body.
pub fn parse_state(body: &str) -> Result<State, SchemaError> {
    let de = &mut serde_json::Deserializer::from_str(body);
    serde_path_to_error::deserialize(de).map_err(|e| SchemaError { path: error_path(&e) })
}

/// The error's path, with the field name added for a missing field (serde
/// reports those at the enclosing object) and array indices written as `[]`.
fn error_path(e: &serde_path_to_error::Error<serde_json::Error>) -> String {
    let base = without_indices(&e.path().to_string());
    let base = if base == "." { String::new() } else { base };
    let message = e.inner().to_string();
    let missing = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split_once('`'))
        .map(|(field, _)| field);
    match missing {
        Some(field) if base.is_empty() => field.to_string(),
        Some(field) => format!("{base}.{field}"),
        None if base.is_empty() => ".".to_string(),
        None => base,
    }
}

/// `defects[3].title` as `defects[].title`: only all-digit brackets are emptied.
fn without_indices(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..=open]);
        rest = &rest[open + 1..];
        if let Some(close) = rest.find(']') {
            let inside = &rest[..close];
            if !inside.is_empty() && inside.bytes().all(|b| b.is_ascii_digit()) {
                rest = &rest[close..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/techlog")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    }

    fn edit(name: &str, f: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut v: serde_json::Value = serde_json::from_str(&fixture(name)).unwrap();
        f(&mut v);
        v.to_string()
    }

    #[test]
    fn parses_go_mel_fixture() {
        let s = parse_state(&fixture("state-fenix-go-mel.json")).unwrap();
        assert_eq!(s.sim.registration.as_deref(), Some("PH-EZA"));
        assert_eq!(s.dispatch.status, DispatchStatus::GoMel);
        assert_eq!(s.defects.len(), 2);
        let af = s.airframe.unwrap();
        assert_eq!(af.family, "fenix-a32x");
        let perf = af.performance.unwrap();
        assert_eq!(perf.fuel_factor_pct, Some(2.3));
        assert_eq!(perf.sim_brief.as_deref(), Some("P02"));
    }

    #[test]
    fn parses_no_go_and_no_airframe_fixtures() {
        let s = parse_state(&fixture("state-no-go.json")).unwrap();
        assert_eq!(s.dispatch.status, DispatchStatus::NoGo);
        let s = parse_state(&fixture("state-no-airframe.json")).unwrap();
        assert!(s.airframe.is_none());
        assert!(s.sim.registration.is_none());
    }

    #[test]
    fn parses_ping() {
        let p: Ping = serde_json::from_str(&fixture("ping.json")).unwrap();
        assert!(p.ok);
        assert_eq!(p.app.as_deref(), Some("TechLog"));
        assert_eq!(p.version.as_deref(), Some("2.1.2"));
        let p: Ping = serde_json::from_str(r#"{"ok":true,"engine":false}"#).unwrap();
        assert_eq!(p.engine, Some(false));
        assert!(p.app.is_none());
    }

    #[test]
    fn missing_dispatch_names_the_field() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v.as_object_mut().unwrap().remove("dispatch");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "dispatch");
    }

    #[test]
    fn renamed_title_points_into_the_defect() {
        let body = edit("state-fenix-go-mel.json", |v| {
            let d = v["defects"][0].as_object_mut().unwrap();
            let title = d.remove("title").unwrap();
            d.insert("name".into(), title);
        });
        let path = parse_state(&body).unwrap_err().path;
        assert_eq!(path, "defects[].title");
    }

    #[test]
    fn a_later_defect_reports_the_same_path() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["defects"][1].as_object_mut().unwrap().remove("title");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "defects[].title");
    }

    #[test]
    fn without_indices_empties_only_numbers() {
        assert_eq!(without_indices("defects[3].title"), "defects[].title");
        assert_eq!(without_indices("a[12].b[0]"), "a[].b[]");
        assert_eq!(without_indices("a[x].b"), "a[x].b");
        assert_eq!(without_indices("a[]"), "a[]");
        assert_eq!(without_indices("dispatch"), "dispatch");
    }

    #[test]
    fn missing_airframe_registration_names_the_field() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["airframe"].as_object_mut().unwrap().remove("registration");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "airframe.registration");
    }

    #[test]
    fn missing_airframe_key_is_not_read_as_no_airframe() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v.as_object_mut().unwrap().remove("airframe");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "airframe");
    }

    #[test]
    fn missing_nullable_defect_key_is_a_schema_error() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["defects"][0].as_object_mut().unwrap().remove("rectifiedUtc");
        });
        let path = parse_state(&body).unwrap_err().path;
        assert_eq!(path, "defects[].rectifiedUtc");
    }

    #[test]
    fn missing_sim_registration_is_a_schema_error() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["sim"].as_object_mut().unwrap().remove("registration");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "sim.registration");
    }

    #[test]
    fn missing_sim_family_is_a_schema_error() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["sim"].as_object_mut().unwrap().remove("family");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "sim.family");
    }

    #[test]
    fn missing_fuel_factor_is_a_schema_error() {
        let body = edit("state-fenix-go-mel.json", |v| {
            let perf = v["airframe"]["performance"].as_object_mut().unwrap();
            perf.remove("fuelFactorPct");
        });
        let path = parse_state(&body).unwrap_err().path;
        assert_eq!(path, "airframe.performance.fuelFactorPct");
    }

    #[test]
    fn unknown_dispatch_status_is_a_schema_error() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["dispatch"]["status"] = "GO·MEL".into();
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "dispatch.status");
    }

    #[test]
    fn null_doubles_parse() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["airframe"]["performance"]["fuelFactorPct"] = serde_json::Value::Null;
        });
        let s = parse_state(&body).unwrap();
        assert_eq!(s.airframe.unwrap().performance.unwrap().fuel_factor_pct, None);
    }
}
