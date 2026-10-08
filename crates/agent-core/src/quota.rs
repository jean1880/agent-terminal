//! Account and plan-quota parsers for the usage indicator, plus the small time helpers it needs.
//!
//! Each agent reports usage differently (Claude: `unifiedWindows` on every turn and a richer
//! `get_usage` on demand; agy: `/usage` buckets holding the REMAINING fraction), and the shapes
//! drift between CLI versions, so every parser is lenient: unknown shapes yield fewer windows,
//! never an error. All of them return the canonical [`QuotaWindow`] with `used` in 0.0-1.0.

use serde_json::Value;

use crate::event::{Account, QuotaWindow};

fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0)
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Follows nested `response` objects until `key` is found (bare payload, `{response: ..}`
/// envelope or a whole recorded frame).
fn find_in_response<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let mut cur = v;
    for _ in 0..4 {
        if let Some(found) = cur.get(key) {
            return Some(found);
        }
        cur = cur.get("response")?;
    }
    None
}

/// Claude's per-turn windows: `rate_limit_event.rate_limit_info`
/// (`unifiedWindows.{five_hour,seven_day}.{utilization 0..1, resetsAt epoch seconds}`).
pub fn claude_turn_windows(rate_limit_info: &Value) -> Vec<QuotaWindow> {
    let Some(windows) = rate_limit_info.get("unifiedWindows") else {
        return Vec::new();
    };
    [("five_hour", "5-hour"), ("seven_day", "Weekly")]
        .into_iter()
        .filter_map(|(key, label)| {
            let w = windows.get(key)?;
            Some(QuotaWindow {
                group: None,
                label: label.to_owned(),
                used: clamp01(w.get("utilization")?.as_f64()?),
                resets_at: w
                    .get("resetsAt")
                    .and_then(Value::as_i64)
                    .map(epoch_to_rfc3339),
            })
        })
        .collect()
}

/// The signed-in account from the `initialize` control response
/// (`response.account {email, organization, subscriptionType, apiProvider}`).
pub fn claude_account(initialize: &Value) -> Option<Account> {
    let account = find_in_response(initialize, "account")?;
    let label = text(account, "email").or_else(|| text(account, "organization"))?;
    Some(Account {
        label,
        plan: text(account, "subscriptionType"),
        provider: text(account, "apiProvider"),
    })
}

/// Claude's `get_usage` reply: `rate_limits.limits[] {kind, percent 0..100, resets_at,
/// scope.model.display_name}`.
pub fn claude_usage(usage: &Value) -> Vec<QuotaWindow> {
    let Some(limits) = find_in_response(usage, "rate_limits")
        .and_then(|r| r.get("limits"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    limits
        .iter()
        .filter_map(|l| {
            let scope = l
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str);
            let label = match (l.get("kind")?.as_str()?, scope) {
                ("session", _) => "5-hour".to_owned(),
                ("weekly_all", _) => "Weekly".to_owned(),
                ("weekly_scoped", Some(model)) => format!("Weekly ({model})"),
                ("weekly_scoped", None) => "Weekly (scoped)".to_owned(),
                _ => return None,
            };
            Some(QuotaWindow {
                group: None,
                label,
                used: clamp01(l.get("percent")?.as_f64()? / 100.0),
                resets_at: text(l, "resets_at"),
            })
        })
        .collect()
}

/// agy's `-p /usage --output-format json`: `command.data.groups[].buckets[]
/// {window weekly|5h, remaining_fraction, reset_time}`; used is `1 - remaining`.
pub fn agy_usage(usage: &Value) -> Vec<QuotaWindow> {
    let Some(groups) = usage
        .pointer("/command/data/groups")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    groups
        .iter()
        .flat_map(|g| {
            let group = text(g, "name");
            g.get("buckets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |b| {
                    let label = match b.get("window")?.as_str()? {
                        "weekly" => "Weekly",
                        "5h" => "5-hour",
                        _ => return None,
                    };
                    Some(QuotaWindow {
                        group: group.clone(),
                        label: label.to_owned(),
                        used: clamp01(1.0 - b.get("remaining_fraction")?.as_f64()?),
                        resets_at: text(b, "reset_time"),
                    })
                })
        })
        .collect()
}

/// agy's signed-in account from `~/.gemini/google_accounts.json`: ONLY the `active` field is
/// read (it is an email address, not a credential).
pub fn agy_account(google_accounts: &Value) -> Option<Account> {
    Some(Account {
        label: text(google_accounts, "active")?,
        plan: None,
        provider: Some("Google".to_owned()),
    })
}

/// Codex's `account/read` result: `account {type: chatgpt, email, planType}` or `{type: apiKey}`
/// (`null` when signed out). The label is the email, or "API key".
pub fn codex_account(result: &Value) -> Option<Account> {
    let account = result.get("account").filter(|a| a.is_object())?;
    match account.get("type").and_then(Value::as_str)? {
        "chatgpt" => Some(Account {
            label: text(account, "email").unwrap_or_else(|| "ChatGPT".to_owned()),
            plan: text(account, "planType"),
            provider: Some("ChatGPT".to_owned()),
        }),
        "apiKey" => Some(Account {
            label: "API key".to_owned(),
            plan: None,
            provider: Some("OpenAI".to_owned()),
        }),
        _ => None,
    }
}

/// Whether Claude's `initialize` reply says no one is signed in. A fresh install (Claude Code
/// 2.1, captured live) answers `account {tokenSource: "none", apiProvider}` with no email; a
/// signed-in one names the account and has no `tokenSource` of "none". An API key or token in
/// the environment shows as another source, not "none".
pub fn claude_signed_out(initialize: &Value) -> bool {
    find_in_response(initialize, "account")
        .and_then(|a| a.get("tokenSource"))
        .and_then(Value::as_str)
        == Some("none")
}

/// Whether output from agy (`agy -p /usage`) says no one is signed in. A fresh install prints
/// "Authentication required. Please visit the URL to log in:" (captured live), then opens a
/// browser and waits for the login, so the app must not keep probing it.
pub fn agy_signed_out(output: &str) -> bool {
    output
        .to_ascii_lowercase()
        .contains("authentication required")
}

/// Whether an `account/read` result says no one is signed in to Codex: no account, while the
/// server needs OpenAI auth. Then every turn fails with "401 Unauthorized" and no usage can be
/// read, so the app says so rather than hanging or hiding the agent.
pub fn codex_signed_out(result: &Value) -> bool {
    let no_account = result.get("account").is_none_or(Value::is_null);
    let needs_auth = result
        .get("requiresOpenaiAuth")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    no_account && needs_auth
}

/// A window's label from its length: 300 min is the "5-hour" window, a week is "Weekly".
fn codex_window_label(mins: Option<i64>, fallback: &str) -> String {
    match mins {
        Some(300) => "5-hour".to_owned(),
        Some(10_080) => "Weekly".to_owned(),
        Some(m) if m > 0 && m % 1440 == 0 => format!("{}-day", m / 1440),
        Some(m) if m > 0 && m % 60 == 0 => format!("{}-hour", m / 60),
        Some(m) if m > 0 => format!("{m}-min"),
        _ => fallback.to_owned(),
    }
}

fn codex_snapshot_windows(snapshot: &Value, group: Option<String>) -> Vec<QuotaWindow> {
    [("primary", "Primary"), ("secondary", "Secondary")]
        .into_iter()
        .filter_map(|(key, fallback)| {
            let w = snapshot.get(key).filter(|w| w.is_object())?;
            Some(QuotaWindow {
                group: group.clone(),
                label: codex_window_label(
                    w.get("windowDurationMins").and_then(Value::as_i64),
                    fallback,
                ),
                used: clamp01(w.get("usedPercent")?.as_f64()? / 100.0),
                resets_at: w
                    .get("resetsAt")
                    .and_then(Value::as_i64)
                    .map(epoch_to_rfc3339),
            })
        })
        .collect()
}

/// Codex's plan windows from an `account/rateLimits/read` result, or from the params of an
/// `account/rateLimits/updated` notification (`{rateLimits: snapshot}`). The multi-bucket view
/// wins when present, one group per metered limit (the plain `codex` bucket has no group);
/// otherwise the single-bucket `rateLimits` is used. `usedPercent` is 0-100.
pub fn codex_rate_limits(result: &Value) -> Vec<QuotaWindow> {
    if let Some(buckets) = result
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .filter(|b| !b.is_empty())
    {
        let mut keys: Vec<&String> = buckets.keys().collect();
        keys.sort();
        return keys
            .into_iter()
            .flat_map(|key| {
                let snapshot = &buckets[key];
                let group = if key == "codex" {
                    None
                } else {
                    Some(text(snapshot, "limitName").unwrap_or_else(|| key.clone()))
                };
                codex_snapshot_windows(snapshot, group)
            })
            .collect();
    }
    result
        .get("rateLimits")
        .map(|s| codex_snapshot_windows(s, None))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Time helpers (no chrono: the UI needs only epoch seconds and an "in 3 h 47 min" phrase)
// ---------------------------------------------------------------------------------------------

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Epoch seconds as an RFC 3339 UTC timestamp (`2026-10-06T16:30:00Z`).
pub fn epoch_to_rfc3339(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Epoch seconds of an RFC 3339 timestamp (`Z` or `+hh:mm`, optional fraction). `None` for
/// anything else.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut dp = date.splitn(3, '-');
    let (y, m, d) = (
        dp.next()?.parse::<i64>().ok()?,
        dp.next()?.parse::<i64>().ok()?,
        dp.next()?.parse::<i64>().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Split the time from its zone designator.
    let zone_at = rest.find(['Z', 'z', '+', '-']);
    let (time, zone) = match zone_at {
        Some(i) => rest.split_at(i),
        None => (rest, "Z"),
    };
    let time = time.split('.').next()?;
    let mut tp = time.splitn(3, ':');
    let (hh, mm, ss) = (
        tp.next()?.parse::<i64>().ok()?,
        tp.next()?.parse::<i64>().ok()?,
        tp.next().map_or(Some(0), |v| v.parse::<i64>().ok())?,
    );
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let offset = match zone {
        "Z" | "z" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let (oh, om) = z[1..].split_once(':')?;
            sign * (oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60)
        }
    };
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

/// "resets in 3 h 47 min" style phrase for a reset `secs_left` seconds away. Past or zero:
/// "resets now".
pub fn resets_in_text(secs_left: i64) -> String {
    if secs_left <= 0 {
        return "resets now".to_owned();
    }
    let mins = (secs_left + 59) / 60; // round up: never say "0 min"
    let (d, h, m) = (mins / 1440, mins % 1440 / 60, mins % 60);
    let body = match (d, h, m) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h} h"),
        (0, h, m) => format!("{h} h {m} min"),
        (d, 0, _) => format!("{d} d"),
        (d, h, _) => format!("{d} d {h} h"),
    };
    format!("resets in {body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_account_reads_chatgpt_and_api_key_and_signed_out() {
        let a = codex_account(
            &json!({"account": {"type": "chatgpt", "email": "u@example.com",
            "planType": "plus"}, "requiresOpenaiAuth": true}),
        )
        .expect("account");
        assert_eq!(
            (a.label.as_str(), a.plan.as_deref(), a.provider.as_deref()),
            ("u@example.com", Some("plus"), Some("ChatGPT"))
        );
        let k = codex_account(&json!({"account": {"type": "apiKey"}})).expect("key");
        assert_eq!(k.label, "API key");
        assert_eq!(codex_account(&json!({"account": null})), None);
        assert_eq!(codex_account(&json!({})), None);
        assert_eq!(codex_account(&json!({"account": {"type": "other"}})), None);
    }

    /// Captured live: a fresh Claude Code's `initialize` account, and a signed-in one's shape.
    #[test]
    fn claude_signed_out_is_token_source_none() {
        let fresh = json!({"type": "control_response", "response": {"subtype": "success",
            "response": {"account": {"tokenSource": "none", "apiProvider": "firstParty"}}}});
        assert!(claude_signed_out(&fresh));
        let signed_in = json!({"type": "control_response", "response": {"subtype": "success",
            "response": {"account": {"email": "u@example.com", "organization": "Org",
            "subscriptionType": "max", "apiProvider": "firstParty"}}}});
        assert!(!claude_signed_out(&signed_in));
        assert!(
            !claude_signed_out(&json!({})),
            "no account at all is not proof"
        );
    }

    #[test]
    fn agy_signed_out_reads_its_login_prompt() {
        assert!(agy_signed_out(
            "Authentication required. Please visit the URL to log in:\n  https://accounts…"
        ));
        assert!(!agy_signed_out(
            r#"{"status":"SUCCESS","response":"Gemini Models…"}"#
        ));
    }

    /// The shape a signed-out `codex app-server` 0.160 answers `account/read` with (captured
    /// live): no account, and auth required.
    #[test]
    fn codex_signed_out_is_no_account_while_auth_is_required() {
        assert!(codex_signed_out(&json!(
            {"account": null, "requiresOpenaiAuth": true, "workspaceRouting": null}
        )));
        assert!(!codex_signed_out(&json!(
            {"account": {"type": "chatgpt", "email": "u@example.com"}, "requiresOpenaiAuth": true}
        )));
        assert!(
            !codex_signed_out(&json!({"account": null, "requiresOpenaiAuth": false})),
            "a server that needs no OpenAI auth (another provider) is not signed out"
        );
        assert!(!codex_signed_out(&json!({})));
    }

    #[test]
    fn codex_rate_limits_label_windows_by_length_and_scale_percent() {
        let single = json!({"rateLimits": {
            "primary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 1_790_000_000},
            "secondary": {"usedPercent": 140, "windowDurationMins": 10080, "resetsAt": null}}});
        let w = codex_rate_limits(&single);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].label.as_str(), w[0].used), ("5-hour", 0.25));
        assert!(w[0].resets_at.as_deref().is_some_and(|r| r.ends_with('Z')));
        assert_eq!((w[1].label.as_str(), w[1].used), ("Weekly", 1.0));
        assert_eq!(w[1].resets_at, None);

        // The notification shape is the same snapshot; a missing length falls back to the slot.
        let note = json!({"rateLimits": {"primary": {"usedPercent": 5}}});
        assert_eq!(codex_rate_limits(&note)[0].label, "Primary");

        // Buckets win over the single view; the `codex` bucket has no group.
        let multi = json!({
            "rateLimits": {"primary": {"usedPercent": 1, "windowDurationMins": 300}},
            "rateLimitsByLimitId": {
                "codex": {"primary": {"usedPercent": 10, "windowDurationMins": 300}},
                "codex_x": {"limitName": "GPT-X", "secondary": {"usedPercent": 50,
                    "windowDurationMins": 2880}}}});
        let w = codex_rate_limits(&multi);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].group.clone(), w[0].used), (None, 0.1));
        assert_eq!(
            (w[1].group.as_deref(), w[1].label.as_str()),
            (Some("GPT-X"), "2-day")
        );
        assert!(codex_rate_limits(&json!({})).is_empty());
    }

    const TURN: &str = include_str!("../tests/fixtures/claude-turn-approval.ndjson");
    const CONTROLS: &str = include_str!("../tests/fixtures/claude-controls-slash.ndjson");

    fn frames(fixture: &str) -> impl Iterator<Item = Value> + '_ {
        fixture
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|r| r["frame"].clone())
    }

    /// Shape of a real `get_usage` reply, scrubbed (no identity in it).
    fn usage_reply() -> Value {
        json!({"response": {"request_id": "usage-1", "subtype": "success", "response": {
        "subscription_type": "pro",
        "rate_limits": {"limits": [
            {"group": "session", "kind": "session", "percent": 50,
             "resets_at": "2026-10-06T16:30:00.057599+00:00", "scope": null},
            {"group": "weekly", "kind": "weekly_all", "percent": 49,
             "resets_at": "2026-10-10T22:00:00.057620+00:00", "scope": null},
            {"group": "weekly", "kind": "weekly_scoped", "percent": 0,
             "resets_at": "2026-10-10T22:00:00+00:00",
             "scope": {"model": {"display_name": "Fable", "id": null}, "surface": null}},
            {"kind": "mystery", "percent": 9}
        ]}}}})
    }

    fn agy_reply() -> Value {
        json!({"status": "SUCCESS", "response": "ignored text", "command": {"name": "usage", "data": {
        "groups": [
            {"name": "Gemini Models", "buckets": [
                {"window": "weekly", "remaining_fraction": 0.862052857875824,
                 "reset_time": "2026-10-09T23:45:50Z"},
                {"window": "5h", "remaining_fraction": 0.9938423037528992,
                 "reset_time": "2026-10-06T16:59:24Z"}]},
            {"name": "Claude and GPT models", "buckets": [
                {"window": "weekly", "remaining_fraction": 1, "reset_time": "2026-10-13T13:13:52Z"},
                {"window": "monthly", "remaining_fraction": 0.5}]}
        ]}}})
    }

    #[test]
    fn claude_turn_windows_from_the_recorded_rate_limit_event() {
        let info = frames(TURN)
            .find(|f| f["type"] == "rate_limit_event")
            .expect("a rate_limit_event")["rate_limit_info"]
            .clone();
        let w = claude_turn_windows(&info);
        assert_eq!(w.len(), 2);
        assert_eq!(
            (w[0].label.as_str(), w[0].group.as_deref()),
            ("5-hour", None)
        );
        assert!((w[0].used - 0.07).abs() < 1e-9);
        assert_eq!(w[0].resets_at.as_deref(), Some("2026-10-06T16:30:00Z"));
        assert_eq!(w[1].label, "Weekly");
        assert!((w[1].used - 0.43).abs() < 1e-9);
        assert!(claude_turn_windows(&json!({"status": "allowed"})).is_empty());
    }

    #[test]
    fn claude_account_from_the_recorded_initialize() {
        let init = frames(CONTROLS)
            .find(|f| f.pointer("/response/request_id") == Some(&json!("init-1")))
            .expect("init-1");
        let a = claude_account(&init).expect("account");
        assert_eq!(a.label, "user@example.com");
        assert_eq!(a.plan.as_deref(), Some("Claude Pro"));
        assert_eq!(a.provider.as_deref(), Some("firstParty"));
        // The inner payload alone works too; no account means None.
        assert_eq!(claude_account(&init["response"]["response"]), Some(a));
        assert_eq!(claude_account(&json!({"models": []})), None);
        assert_eq!(claude_account(&json!({"account": {}})), None);
    }

    #[test]
    fn claude_usage_maps_kinds_and_scales_percent() {
        let w = claude_usage(&usage_reply());
        let got: Vec<_> = w.iter().map(|w| (w.label.as_str(), w.used)).collect();
        assert_eq!(
            got,
            [("5-hour", 0.5), ("Weekly", 0.49), ("Weekly (Fable)", 0.0)]
        );
        assert_eq!(
            w[0].resets_at.as_deref(),
            Some("2026-10-06T16:30:00.057599+00:00")
        );
        assert!(claude_usage(&json!({"nothing": 1})).is_empty());
    }

    #[test]
    fn agy_usage_converts_remaining_to_used_per_group() {
        let w = agy_usage(&agy_reply());
        assert_eq!(w.len(), 3, "the unknown 'monthly' window is skipped");
        assert_eq!(w[0].group.as_deref(), Some("Gemini Models"));
        assert_eq!(w[0].label, "Weekly");
        assert!((w[0].used - 0.137947142124176).abs() < 1e-9);
        assert_eq!(w[1].label, "5-hour");
        assert_eq!(w[2].group.as_deref(), Some("Claude and GPT models"));
        assert_eq!(w[2].used, 0.0);
        assert_eq!(w[2].resets_at.as_deref(), Some("2026-10-13T13:13:52Z"));
        assert!(agy_usage(&json!("not json usage")).is_empty());
    }

    #[test]
    fn agy_account_reads_only_the_active_field() {
        let a = agy_account(&json!({"active": "user@example.com", "old": ["x@y.z"]})).unwrap();
        assert_eq!(a.label, "user@example.com");
        assert_eq!(a.provider.as_deref(), Some("Google"));
        assert_eq!(agy_account(&json!({"active": ""})), None);
        assert_eq!(agy_account(&json!({})), None);
    }

    #[test]
    fn rfc3339_round_trips_and_parses_offsets() {
        for secs in [0, 1_791_304_200, 1_791_669_600, 951_782_400, -86_400] {
            assert_eq!(parse_rfc3339(&epoch_to_rfc3339(secs)), Some(secs), "{secs}");
        }
        assert_eq!(epoch_to_rfc3339(1_791_304_200), "2026-10-06T16:30:00Z");
        assert_eq!(
            parse_rfc3339("2026-10-06T16:30:00.057599+00:00"),
            Some(1_791_304_200)
        );
        assert_eq!(
            parse_rfc3339("2026-10-06T12:30:00-04:00"),
            Some(1_791_304_200)
        );
        assert_eq!(
            parse_rfc3339("2026-10-06T18:30:00+02:00"),
            Some(1_791_304_200)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!(parse_rfc3339("2026-13-06T00:00:00Z"), None);
    }

    #[test]
    fn resets_in_phrases() {
        assert_eq!(resets_in_text(0), "resets now");
        assert_eq!(resets_in_text(-5), "resets now");
        assert_eq!(resets_in_text(1), "resets in 1 min");
        assert_eq!(resets_in_text(45 * 60), "resets in 45 min");
        assert_eq!(resets_in_text(3 * 3600 + 47 * 60), "resets in 3 h 47 min");
        assert_eq!(resets_in_text(2 * 3600), "resets in 2 h");
        assert_eq!(resets_in_text(3 * 86_400 + 10 * 3600), "resets in 3 d 10 h");
        assert_eq!(resets_in_text(86_400), "resets in 1 d");
    }
}
