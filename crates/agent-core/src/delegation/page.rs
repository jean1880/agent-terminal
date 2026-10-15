//! Page cursors and page assembly by bytes (contract §3 page rules, "Selection, then
//! measurement").

use serde_json::Value;

use super::error::{ErrorCode, ToolError};
use super::ids::{Cursor, WorkerId};
use super::mailbox::fnv1a_128;
use super::token::{b64url_decode, b64url_encode};
use super::wire::{escaped_len, transport_cost};

/// What a cursor was issued for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageFilter {
    pub caller: WorkerId,
    /// `read.target` or `list.target`.
    pub target: Option<WorkerId>,
    /// `wait.workers` (order does not matter).
    pub workers: Vec<WorkerId>,
    pub mailbox: bool,
}

/// What a cursor encodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPayload {
    pub root: WorkerId,
    pub stream_epoch: u64,
    pub last_seq: u64,
    pub filter_digest: [u8; 16],
}

const CURSOR_VERSION: u8 = 1;

fn push_field(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_be_bytes());
    buf.extend_from_slice(s.as_bytes());
}

/// A 128-bit digest of the filter, independent of the order of `workers` (hand-written
/// FNV-1a over a length-prefixed encoding; it detects reuse with another filter and is not a
/// secret).
pub fn filter_digest(f: &PageFilter) -> [u8; 16] {
    let mut workers: Vec<&str> = f.workers.iter().map(WorkerId::as_str).collect();
    workers.sort_unstable();
    workers.dedup();
    let mut buf = b"agent-terminal/page-filter/1".to_vec();
    push_field(&mut buf, f.caller.as_str());
    match &f.target {
        Some(t) => {
            buf.push(1);
            push_field(&mut buf, t.as_str());
        }
        None => buf.push(0),
    }
    buf.extend_from_slice(&(workers.len() as u32).to_be_bytes());
    for w in workers {
        push_field(&mut buf, w);
    }
    buf.push(u8::from(f.mailbox));
    fnv1a_128(&buf).to_be_bytes()
}

/// The cursor for `p`: base64url of a versioned binary record, at most 102 bytes.
pub fn encode_cursor(p: &CursorPayload) -> Cursor {
    let root = p.root.as_str().as_bytes();
    let mut buf = Vec::with_capacity(2 + root.len() + 32);
    buf.push(CURSOR_VERSION);
    // A handle is at most 44 bytes.
    buf.push(root.len() as u8);
    buf.extend_from_slice(root);
    buf.extend_from_slice(&p.stream_epoch.to_be_bytes());
    buf.extend_from_slice(&p.last_seq.to_be_bytes());
    buf.extend_from_slice(&p.filter_digest);
    Cursor::from_encoded(b64url_encode(&buf))
}

fn read_u64(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

fn parse_cursor(c: &Cursor) -> Option<CursorPayload> {
    let bytes = b64url_decode(c.as_str())?;
    let (&version, rest) = bytes.split_first()?;
    if version != CURSOR_VERSION {
        return None;
    }
    let (&root_len, rest) = rest.split_first()?;
    let root_len = usize::from(root_len);
    if rest.len() != root_len + 32 {
        return None;
    }
    let root = WorkerId::parse(std::str::from_utf8(&rest[..root_len]).ok()?).ok()?;
    let rest = &rest[root_len..];
    Some(CursorPayload {
        root,
        stream_epoch: read_u64(&rest[..8])?,
        last_seq: read_u64(&rest[8..16])?,
        filter_digest: rest[16..32].try_into().ok()?,
    })
}

/// `InvalidArgument` (field `after`) for a malformed cursor or one issued for another filter;
/// `CursorExpired` for another stream epoch (re-read with no cursor). The caller checks the
/// root.
pub fn decode_cursor(
    c: &Cursor,
    f: &PageFilter,
    stream_epoch: u64,
) -> Result<CursorPayload, ErrorCode> {
    let p = parse_cursor(c).ok_or(ErrorCode::InvalidArgument)?;
    if p.filter_digest != filter_digest(f) {
        return Err(ErrorCode::InvalidArgument);
    }
    if p.stream_epoch != stream_epoch {
        return Err(ErrorCode::CursorExpired);
    }
    Ok(p)
}

/// One unit per seq: a message seq is a message only, never also an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Event,
    Message,
}

/// An upper bound of what a unit or envelope costs; never claimed to be exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reservation {
    pub json: usize,
    pub transport: usize,
}

fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(usize::MAX, |s| s.len())
}

impl Reservation {
    /// `json = len(json(v))`; `transport = len(json(v)) + len(json_string(json(v))) + 4`
    /// (a separator and slack in both copies).
    pub fn unit(v: &Value) -> Reservation {
        let Ok(json) = serde_json::to_string(v) else {
            return Reservation {
                json: usize::MAX,
                transport: usize::MAX,
            };
        };
        Reservation {
            json: json.len(),
            transport: json
                .len()
                .saturating_add(escaped_len(&json))
                .saturating_add(4),
        }
    }

    /// The page with empty unit arrays and every variable envelope field at its maximum
    /// (snapshot, ack, a 256-byte cursor, all flags).
    pub fn max_envelope(page_with_empty_units: &Value) -> Reservation {
        Reservation {
            json: json_len(page_with_empty_units),
            transport: transport_cost(page_with_empty_units),
        }
    }
}

/// Step 1, reservation: units in seq order while `json(snapshot + units) <= max_bytes` and
/// `envelope + units <= budget`. Always takes the first unit if there is one; never skips.
pub fn reserve(
    envelope: Reservation,
    snapshot_json: usize,
    units: &[(u64, UnitKind, Reservation)],
    max_units: u32,
    max_bytes: u32,
    budget: usize,
) -> usize {
    let mut json = snapshot_json;
    let mut transport = envelope.transport;
    let mut taken = 0;
    for (_, _, r) in units {
        if taken > 0
            && (taken >= max_units as usize
                || json.saturating_add(r.json) > max_bytes as usize
                || transport.saturating_add(r.transport) > budget)
        {
            break;
        }
        json = json.saturating_add(r.json);
        transport = transport.saturating_add(r.transport);
        taken += 1;
    }
    taken
}

fn settle_n(
    taken: usize,
    budget: usize,
    build: impl Fn(usize) -> Value,
) -> Result<(Value, usize), ToolError> {
    let mut n = taken;
    loop {
        let page = build(n);
        if transport_cost(&page) <= budget {
            return Ok((page, n));
        }
        if n <= 1 {
            return Err(ToolError::new(
                ErrorCode::InternalError,
                "a page with one unit exceeds the output budget",
            )
            .with_detail("reason", "output_budget"));
        }
        n -= 1;
    }
}

/// Steps 2–4: `build(n)` serialises the real page with the first `n` units, and its measured
/// `transport_cost` decides. Drops the last unit while over budget, never below one. `true`
/// when a unit was dropped. `InternalError` only if the one-unit page is over budget (a bug).
pub fn settle(
    taken: usize,
    budget: usize,
    build: impl Fn(usize) -> Value,
) -> Result<(Value, bool), ToolError> {
    settle_n(taken, budget, build).map(|(page, n)| (page, n < taken))
}

/// One unit of a read or wait page, in seq order.
#[derive(Debug, Clone, PartialEq)]
pub struct PageUnit {
    pub seq: u64,
    pub kind: UnitKind,
    /// The serialised `PageEvent` or `PageMessage`.
    pub value: Value,
}

/// What the page renderer gets for the first `n` units.
#[derive(Debug, Clone, PartialEq)]
pub struct PageParts<'a> {
    pub snapshot: Option<&'a Value>,
    pub events: Vec<&'a Value>,
    pub messages: Vec<&'a Value>,
    /// The seq `next_cursor` resumes after; `None` if no unit is in the page.
    pub last_seq: Option<u64>,
    /// More units were available.
    pub truncated: bool,
}

/// An assembled page.
#[derive(Debug, Clone, PartialEq)]
pub struct Assembled {
    pub page: Value,
    pub taken: usize,
    pub last_seq: Option<u64>,
    pub truncated: bool,
}

/// Contract §3 "Selection, then measurement" for a read or wait page. The snapshot is always
/// in and counts first; units are whole and in seq order; the first unit is always taken;
/// `render` builds the real page (cursor from `last_seq`, `truncated`) and its measured size
/// decides.
pub fn assemble(
    envelope: Reservation,
    snapshot: Option<&Value>,
    units: &[PageUnit],
    max_units: u32,
    max_bytes: u32,
    budget: usize,
    render: impl Fn(&PageParts<'_>) -> Value,
) -> Result<Assembled, ToolError> {
    let reservations: Vec<(u64, UnitKind, Reservation)> = units
        .iter()
        .map(|u| (u.seq, u.kind, Reservation::unit(&u.value)))
        .collect();
    let snapshot_json = snapshot.map_or(0, json_len);
    let taken = reserve(
        envelope,
        snapshot_json,
        &reservations,
        max_units,
        max_bytes,
        budget,
    );
    let parts = |n: usize| {
        let chosen = &units[..n.min(units.len())];
        PageParts {
            snapshot,
            events: chosen
                .iter()
                .filter(|u| u.kind == UnitKind::Event)
                .map(|u| &u.value)
                .collect(),
            messages: chosen
                .iter()
                .filter(|u| u.kind == UnitKind::Message)
                .map(|u| &u.value)
                .collect(),
            last_seq: chosen.last().map(|u| u.seq),
            truncated: n < units.len(),
        }
    };
    let (page, n) = settle_n(taken, budget, |n| render(&parts(n)))?;
    let final_parts = parts(n);
    Ok(Assembled {
        page,
        taken: n,
        last_seq: final_parts.last_seq,
        truncated: final_parts.truncated,
    })
}

/// The same reservation and measurement over `WorkerSummary` values for `list` (at least one
/// worker when any is available). `render(workers, truncated)` builds the real page, whose
/// `next_cursor` resumes at the first worker not taken.
pub fn assemble_list(
    envelope: Reservation,
    workers: &[Value],
    limit: u32,
    budget: usize,
    render: impl Fn(&[&Value], bool) -> Value,
) -> Result<(Value, usize), ToolError> {
    let reservations: Vec<(u64, UnitKind, Reservation)> = workers
        .iter()
        .enumerate()
        .map(|(i, w)| (i as u64, UnitKind::Event, Reservation::unit(w)))
        .collect();
    let taken = reserve(envelope, 0, &reservations, limit, u32::MAX, budget);
    settle_n(taken, budget, |n| {
        let chosen: Vec<&Value> = workers[..n.min(workers.len())].iter().collect();
        render(&chosen, n < workers.len())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::wire::{fixtures, OUTPUT_BUDGET};
    use serde_json::json;

    fn w(s: &str) -> WorkerId {
        WorkerId::parse(&format!("w_{s}")).expect("valid")
    }

    fn filter() -> PageFilter {
        PageFilter {
            caller: w("caller00"),
            target: None,
            workers: vec![w("aaaaaaaa"), w("bbbbbbbb")],
            mailbox: true,
        }
    }

    #[test]
    fn cursors_round_trip_and_bind_to_their_filter_and_epoch() {
        let f = filter();
        let p = CursorPayload {
            root: w("rootroot"),
            stream_epoch: 7,
            last_seq: 42,
            filter_digest: filter_digest(&f),
        };
        let c = encode_cursor(&p);
        assert!(c.as_str().len() <= 256);
        assert_eq!(decode_cursor(&c, &f, 7), Ok(p.clone()));
        let mut reordered = f.clone();
        reordered.workers.reverse();
        assert_eq!(filter_digest(&reordered), filter_digest(&f));
        assert_eq!(decode_cursor(&c, &reordered, 7), Ok(p));
        let mut other = f.clone();
        other.mailbox = false;
        assert_eq!(
            decode_cursor(&c, &other, 7),
            Err(ErrorCode::InvalidArgument)
        );
        let mut targeted = f.clone();
        targeted.target = Some(w("aaaaaaaa"));
        assert_ne!(filter_digest(&targeted), filter_digest(&f));
        assert_eq!(decode_cursor(&c, &f, 8), Err(ErrorCode::CursorExpired));
        let junk = Cursor::parse("not-a-cursor").expect("syntax");
        assert_eq!(decode_cursor(&junk, &f, 7), Err(ErrorCode::InvalidArgument));
        // The longest handle still fits.
        let long = CursorPayload {
            root: WorkerId::parse(&format!("w_{}", "z".repeat(40))).expect("valid"),
            stream_epoch: u64::MAX,
            last_seq: u64::MAX,
            filter_digest: [0xff; 16],
        };
        let lc = encode_cursor(&long);
        assert!(lc.as_str().len() <= 102);
        assert_eq!(parse_cursor(&lc), Some(long));
    }

    fn unit(seq: u64, kind: UnitKind, text: &str) -> PageUnit {
        PageUnit {
            seq,
            kind,
            value: json!({"seq": seq, "text": text}),
        }
    }

    fn render(p: &PageParts<'_>) -> Value {
        json!({
            "api_version": 1,
            "snapshot": p.snapshot,
            "events": p.events,
            "messages": p.messages,
            "next_cursor": p.last_seq.map(|s| format!("c{s}")).unwrap_or_else(|| "c0".into()),
            "truncated": p.truncated,
            "epoch_changed": false,
            "ack": null
        })
    }

    fn envelope(snapshot: Option<&Value>, ack: Option<&Value>) -> Reservation {
        Reservation::max_envelope(&json!({
            "api_version": 1, "snapshot": snapshot, "events": [], "messages": [],
            "next_cursor": "c".repeat(256), "truncated": true, "epoch_changed": true,
            "ack": ack, "timed_out": true
        }))
    }

    fn seqs(page: &Value) -> Vec<u64> {
        let mut out: Vec<u64> = ["events", "messages"]
            .iter()
            .flat_map(|k| page[*k].as_array().cloned().unwrap_or_default())
            .filter_map(|u| u["seq"].as_u64())
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn assemble_counts_units_once_and_never_skips() {
        let snapshot = json!({"worker": "s"});
        let units = vec![
            unit(1, UnitKind::Event, "a"),
            unit(2, UnitKind::Message, "b"),
            unit(3, UnitKind::Event, "c"),
            unit(4, UnitKind::Message, "d"),
        ];
        let env = envelope(Some(&snapshot), None);
        let a = assemble(
            env,
            Some(&snapshot),
            &units,
            2,
            65_536,
            OUTPUT_BUDGET,
            render,
        )
        .expect("page");
        assert_eq!(a.taken, 2);
        assert_eq!(a.page["events"].as_array().map(Vec::len), Some(1));
        assert_eq!(a.page["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(a.page["snapshot"], snapshot);
        assert!(a.truncated);
        assert_eq!(a.last_seq, Some(2));
        // The next page starts at the event after the message: nothing is skipped.
        let rest: Vec<PageUnit> = units.iter().filter(|u| u.seq > 2).cloned().collect();
        let b = assemble(
            env,
            Some(&snapshot),
            &rest,
            1,
            65_536,
            OUTPUT_BUDGET,
            render,
        )
        .expect("page");
        assert_eq!(seqs(&b.page), vec![3]);
        // The snapshot counts first against max_bytes, but the first unit is always taken.
        let big = unit(9, UnitKind::Message, &"x".repeat(5_000));
        let c = assemble(
            env,
            Some(&snapshot),
            &[big.clone(), unit(10, UnitKind::Event, "y")],
            100,
            1024,
            OUTPUT_BUDGET,
            render,
        )
        .expect("page");
        assert_eq!(c.taken, 1);
        assert!(c.truncated);
        // Everything fits: not truncated.
        let d = assemble(env, None, &units, 100, 65_536, OUTPUT_BUDGET, render).expect("page");
        assert_eq!(d.taken, 4);
        assert!(!d.truncated);
        assert_eq!(d.page["snapshot"], Value::Null);
        // No units: an empty page.
        let e = assemble(
            env,
            Some(&snapshot),
            &[],
            100,
            65_536,
            OUTPUT_BUDGET,
            render,
        )
        .expect("page");
        assert_eq!(e.taken, 0);
        assert_eq!(e.last_seq, None);
    }

    fn text(c: char, bytes: usize) -> String {
        std::iter::repeat_n(c, bytes / c.len_utf8()).collect()
    }

    #[test]
    fn maximal_page_stays_within_the_budget() {
        let snapshot = serde_json::to_value(fixtures::worst_summary('\u{1}')).expect("ser");
        assert!(json_len(&snapshot) <= crate::delegation::wire::MAX_SUMMARY_BYTES);
        let ack = serde_json::to_value(fixtures::worst_ack('\u{1}')).expect("ser");
        assert!(json_len(&ack) <= crate::delegation::wire::MAX_ACK_BYTES);
        let units: Vec<PageUnit> = (1..=20)
            .map(|seq| PageUnit {
                seq,
                kind: UnitKind::Message,
                value: serde_json::to_value(fixtures::worst_message(seq, '\u{1}')).expect("ser"),
            })
            .collect();
        let env = envelope(Some(&snapshot), Some(&ack));
        let render_with_ack = |p: &PageParts<'_>| {
            let mut v = render(p);
            v["ack"] = ack.clone();
            v
        };
        let a = assemble(
            env,
            Some(&snapshot),
            &units,
            500,
            262_144,
            OUTPUT_BUDGET,
            render_with_ack,
        )
        .expect("page");
        assert!(transport_cost(&a.page) <= OUTPUT_BUDGET);
        assert!(a.truncated);
        assert!(a.taken >= 1);
        assert_eq!(seqs(&a.page), (1..=a.taken as u64).collect::<Vec<_>>());
    }

    #[test]
    fn a_maximal_first_unit_is_kept_for_every_text_shape() {
        let ack = serde_json::to_value(fixtures::worst_ack('\u{1}')).expect("ser");
        let snapshot = serde_json::to_value(fixtures::worst_summary('\u{1}')).expect("ser");
        let env = envelope(Some(&snapshot), Some(&ack));
        for c in ['a', '\u{1}', 'é'] {
            // A first unit of about 100 KiB of JSON.
            let per = serde_json::to_string(&c.to_string()).expect("ser").len() - 2;
            let first = PageUnit {
                seq: 1,
                kind: UnitKind::Message,
                value: json!({"seq": 1, "text": text(c, 100 * 1024 / per * c.len_utf8())}),
            };
            assert!(json_len(&first.value) <= 100 * 1024 + 64);
            let units = vec![first, unit(2, UnitKind::Event, "next")];
            let render_with_ack = |p: &PageParts<'_>| {
                let mut v = render(p);
                v["ack"] = ack.clone();
                v
            };
            let a = assemble(
                env,
                Some(&snapshot),
                &units,
                500,
                262_144,
                OUTPUT_BUDGET,
                render_with_ack,
            )
            .expect("page");
            assert!(a.taken >= 1, "{c:?}");
            assert_eq!(a.last_seq.map(|s| s >= 1), Some(true));
            assert!(transport_cost(&a.page) <= OUTPUT_BUDGET, "{c:?}");
        }
    }

    #[test]
    fn conservative_reservation_still_advances() {
        // The reservation assumes a 64 KiB ack and a 256-byte cursor; the real page has
        // neither, so it stops earlier than what would fit, but always advances.
        let ack = serde_json::to_value(fixtures::worst_ack('\u{1}')).expect("ser");
        let env = envelope(None, Some(&ack));
        let units: Vec<PageUnit> = (1..=60)
            .map(|seq| unit(seq, UnitKind::Event, &text('\u{1}', 2 * 1024)))
            .collect();
        let reservations: Vec<_> = units
            .iter()
            .map(|u| (u.seq, u.kind, Reservation::unit(&u.value)))
            .collect();
        let taken = reserve(env, 0, &reservations, 500, 262_144 * 4, OUTPUT_BUDGET);
        let a = assemble(env, None, &units, 500, 262_144 * 4, OUTPUT_BUDGET, render).expect("page");
        assert_eq!(a.taken, taken);
        assert!(a.taken >= 1 && a.taken < units.len());
        assert_eq!(a.last_seq, Some(a.taken as u64));
        let cost = transport_cost(&a.page);
        assert!(cost <= OUTPUT_BUDGET);
        // One more unit would really have fitted: the stop was conservative.
        let refs: Vec<&Value> = units[..=a.taken].iter().map(|u| &u.value).collect();
        let real_next = render(&PageParts {
            snapshot: None,
            events: refs,
            messages: vec![],
            last_seq: Some(a.taken as u64 + 1),
            truncated: true,
        });
        assert!(
            transport_cost(&real_next) <= OUTPUT_BUDGET,
            "the reservation was not conservative here"
        );
    }

    #[test]
    fn settle_corrects_an_underestimate_from_the_end_only() {
        let units: Vec<Value> = (1..=20)
            .map(|seq| json!({"seq": seq, "text": text('\u{1}', 30 * 1024)}))
            .collect();
        let cheap = vec![
            (
                0,
                UnitKind::Event,
                Reservation {
                    json: 1,
                    transport: 1
                }
            );
            units.len()
        ];
        let taken = reserve(
            Reservation::default(),
            0,
            &cheap,
            500,
            u32::MAX,
            OUTPUT_BUDGET,
        );
        assert_eq!(taken, 20);
        let build = |n: usize| json!({"units": units[..n].to_vec(), "truncated": n < units.len()});
        let (page, dropped) = settle(taken, OUTPUT_BUDGET, build).expect("settled");
        assert!(dropped);
        assert_eq!(page["truncated"], true);
        assert!(transport_cost(&page) <= OUTPUT_BUDGET);
        let kept: Vec<u64> = page["units"]
            .as_array()
            .map(|a| a.iter().filter_map(|u| u["seq"].as_u64()).collect())
            .unwrap_or_default();
        assert!(!kept.is_empty());
        assert_eq!(kept, (1..=kept.len() as u64).collect::<Vec<_>>());
    }

    #[test]
    fn a_one_unit_page_over_budget_is_an_internal_error() {
        let e = settle(1, 1000, |_| json!({"x": "y".repeat(2000)})).expect_err("bug");
        assert_eq!(e.code, ErrorCode::InternalError);
        assert!(!e.retryable);
        assert_eq!(
            e.details.and_then(|d| d.get("reason").cloned()),
            Some(json!("output_budget"))
        );
    }

    #[test]
    fn maximal_list_keeps_a_prefix_within_the_budget() {
        let workers: Vec<Value> = (0..100)
            .map(|i| {
                let mut s = fixtures::worst_summary('\u{1}');
                s.worker_id = WorkerId::parse(&format!("w_{i:08}")).expect("valid");
                serde_json::to_value(s).expect("ser")
            })
            .collect();
        let env = Reservation::max_envelope(&json!({
            "api_version": 1, "workers": [], "next_cursor": "c".repeat(256), "truncated": true
        }));
        let render = |ws: &[&Value], truncated: bool| json!({"api_version": 1, "workers": ws, "next_cursor": "c1", "truncated": truncated});
        let (page, taken) = assemble_list(env, &workers, 100, OUTPUT_BUDGET, render).expect("list");
        assert!((1..100).contains(&taken));
        assert_eq!(page["truncated"], true);
        assert!(transport_cost(&page) <= OUTPUT_BUDGET);
        let ids: Vec<&str> = page["workers"]
            .as_array()
            .map(|a| a.iter().filter_map(|w| w["worker_id"].as_str()).collect())
            .unwrap_or_default();
        let expected: Vec<String> = (0..taken).map(|i| format!("w_{i:08}")).collect();
        assert_eq!(ids, expected);
    }
}
