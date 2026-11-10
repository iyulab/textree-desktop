//! The `app.launch` envelope (Application Insights event telemetry) and an RFC3339 timestamp.
//! Pure: takes the event, key, facts and a timestamp string in, returns JSON. The only fields
//! written are the by-construction allowlist.

use crate::telemetry::event::{EnvFacts, LaunchEvent};
use serde_json::{json, Value};

pub fn launch_envelope(
    _event: &LaunchEvent,
    instrumentation_key: &str,
    facts: &EnvFacts,
    timestamp: &str,
) -> Value {
    json!({
        "name": "Microsoft.ApplicationInsights.Event",
        "time": timestamp,
        "iKey": instrumentation_key,
        "tags": { "ai.cloud.role": "textree-desktop", "ai.application.ver": facts.app_version },
        "data": {
            "baseType": "EventData",
            "baseData": {
                "ver": 2,
                "name": LaunchEvent::NAME,
                "properties": { "appVersion": facts.app_version, "os": facts.os, "arch": facts.arch },
            }
        }
    })
}

/// Formats epoch seconds as `YYYY-MM-DDTHH:MM:SS.000Z` (UTC) without any date dependency.
/// Civil-from-days per Howard Hinnant's algorithm.
pub fn format_rfc3339_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> EnvFacts {
        EnvFacts {
            app_version: "0.2.4".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
        }
    }

    #[test]
    fn rfc3339_epoch_zero() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn rfc3339_known_instant() {
        // 2001-09-09T01:46:40Z
        assert_eq!(
            format_rfc3339_utc(1_000_000_000),
            "2001-09-09T01:46:40.000Z"
        );
    }

    #[test]
    fn the_launch_envelope_has_the_event_shape_and_facts() {
        let env = launch_envelope(&LaunchEvent, "ikey", &facts(), "2026-07-01T00:00:00.000Z");
        assert_eq!(env["name"], "Microsoft.ApplicationInsights.Event");
        assert_eq!(env["iKey"], "ikey");
        assert_eq!(env["time"], "2026-07-01T00:00:00.000Z");
        assert_eq!(env["data"]["baseType"], "EventData");
        assert_eq!(env["data"]["baseData"]["name"], "app.launch");
        let props = &env["data"]["baseData"]["properties"];
        assert_eq!(props["appVersion"], "0.2.4");
        assert_eq!(props["os"], "windows");
        assert_eq!(props["arch"], "x86_64");
    }

    /// Regression guard: no PII-shaped key may appear in the serialized envelope.
    #[test]
    fn no_forbidden_keys_in_the_envelope() {
        let s = serde_json::to_string(&launch_envelope(&LaunchEvent, "k", &facts(), "t")).unwrap();
        for forbidden in [
            "\"path\"",
            "\"message\"",
            "\"query\"",
            "\"title\"",
            "\"vault\"",
            "\"fileName\"",
            "\"content\"",
        ] {
            assert!(
                !s.contains(forbidden),
                "launch envelope leaked forbidden key {forbidden}: {s}"
            );
        }
    }
}
