//! The parts of TechLog's local HTTP API that we read.
//!
//! TechLog writes camelCase keys and always writes nulls. Only the fields the
//! snapshot needs are modelled; every other key is ignored, so TechLog can add
//! fields without breaking us. Any TechLog double may be `null` (NaN and
//! Infinity are written as `null`), so doubles are `Option<f64>`.

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
    pub airframe: Option<Airframe>,
    pub dispatch: Dispatch,
    /// Open and deferred defects only.
    pub defects: Vec<Defect>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sim {
    /// Registration of the aircraft identified in the sim; null when none.
    pub registration: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Airframe {
    pub registration: String,
    pub family: String,
    pub family_label: Option<String>,
    pub variant: Option<String>,
    pub engine: Option<String>,
    pub performance: Option<Perf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Perf {
    /// Total fuel burn penalty in percent, one decimal.
    pub fuel_factor_pct: Option<f64>,
    /// SimBrief perf code such as `"P02"` or `"M01"`.
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
    pub mel_id: Option<String>,
    pub ata: Option<i64>,
    pub title: String,
    /// `"A"`..`"D"` or `"NO-GO"`; anything else is dropped to null in the snapshot.
    pub category: Option<String>,
    /// PascalCase: `"Open"`, `"Deferred"`.
    pub status: String,
    pub raised_utc: Option<String>,
    /// Local date `yyyy-MM-dd`.
    pub deferred_until: Option<String>,
    pub placard: Option<String>,
    pub restrictions: Option<String>,
    pub ops: Option<String>,
    pub position: Option<String>,
    pub label: Option<String>,
    pub performance_note: Option<String>,
    pub rectified_utc: Option<String>,
}

/// TechLog's JSON did not have the shape we expect. `path` is the JSON path
/// of the first problem, such as `dispatch` or `defects[0].title`.
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
/// reports those at the enclosing object).
fn error_path(e: &serde_path_to_error::Error<serde_json::Error>) -> String {
    let base = e.path().to_string();
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
        assert!(path.starts_with("defects[0]"), "{path}");
        assert_eq!(path, "defects[0].title");
    }

    #[test]
    fn missing_airframe_registration_names_the_field() {
        let body = edit("state-fenix-go-mel.json", |v| {
            v["airframe"].as_object_mut().unwrap().remove("registration");
        });
        assert_eq!(parse_state(&body).unwrap_err().path, "airframe.registration");
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
