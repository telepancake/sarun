//! End-to-end acceptance for the IETF-drafts mirror against a scripted
//! stand-in for www.ietf.org speaking the REAL `all_id.txt` shape: ONE
//! line per draft, at its LATEST revision — history is enumerated
//! (`00..NN`), not listed. Real-effect assertions:
//!   - a fresh import of a draft listed at -03 fetches 00..03; a 404'd
//!     revision is watermarked missing and never re-tried; history
//!     reads back newest-first with exact bytes;
//!   - an index head bump fetches ONLY the new revisions;
//!   - an idempotent second run performs zero content GETs;
//!   - a transient 500 is retried to success; a persistent 500 fails
//!     the run loudly without corrupting watermarks (re-run resumes);
//!   - index validators (ETag) make an unchanged index a 304
//!     short-circuit — but only after a FULLY successful pass;
//!   - content GETs are paced by the configured delay;
//!   - a legacy heads-only chain is rebuilt in order when backfilled;
//!   - reopen from disk serves the same data (durability).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ietf_mirror::{FetchConfig, Mirror, MirrorConfig};
use reqwest::blocking::Client;
use tempfile::TempDir;

/// One recorded request.
#[derive(Debug, Clone)]
struct Hit {
    path: String,
    if_none_match: Option<String>,
    at: Instant,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn ok(body: &str) -> Reply {
        Reply { status: 200, headers: vec![], body: body.as_bytes().to_vec() }
    }
    fn status(code: u16) -> Reply {
        Reply { status: code, headers: vec![], body: vec![] }
    }
    fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

type Handler = Box<dyn FnMut(&Hit) -> Reply + Send>;

/// Minimal scripted HTTP/1.1 stand-in: real sockets, per-path handler
/// closures (a route can 500-then-200, or answer If-None-Match with
/// 304), and a request log with arrival timestamps.
struct StandIn {
    routes: Arc<Mutex<HashMap<String, Handler>>>,
    log: Arc<Mutex<Vec<Hit>>>,
    base_url: String,
}

impl StandIn {
    fn start() -> StandIn {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<Mutex<HashMap<String, Handler>>> = Arc::default();
        let log: Arc<Mutex<Vec<Hit>>> = Arc::default();
        {
            let (routes, log) = (routes.clone(), log.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(stream) = conn else { break };
                    let _ = serve_one(stream, &routes, &log);
                }
            });
        }
        StandIn { routes, log, base_url }
    }

    fn route(&self, path: &str, h: impl FnMut(&Hit) -> Reply + Send + 'static) {
        self.routes.lock().unwrap().insert(path.to_string(), Box::new(h));
    }

    /// Serve `body` at `/archive/id/<docname>.txt`.
    fn text(&self, docname: &str, body: &'static str) {
        self.route(&format!("/archive/id/{docname}.txt"), move |_| Reply::ok(body));
    }

    fn hits(&self, path: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|h| h.path == path).count()
    }

    /// All `/archive/id/…` requests, in arrival order.
    fn content_hits(&self) -> Vec<Hit> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.path.starts_with("/archive/"))
            .cloned()
            .collect()
    }

    fn last_hit(&self, path: &str) -> Option<Hit> {
        self.log.lock().unwrap().iter().rev().find(|h| h.path == path).cloned()
    }
}

fn serve_one(
    stream: TcpStream,
    routes: &Mutex<HashMap<String, Handler>>,
    log: &Mutex<Vec<Hit>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut if_none_match = None;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("if-none-match") {
                if_none_match = Some(v.trim().to_string());
            }
        }
    }
    let hit = Hit { path: path.clone(), if_none_match, at: Instant::now() };
    log.lock().unwrap().push(hit.clone());
    let reply = match routes.lock().unwrap().get_mut(&path) {
        Some(h) => h(&hit),
        None => Reply::status(404),
    };
    let reason = match reply.status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut out = stream;
    write!(
        out,
        "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        reply.status,
        reason,
        reply.body.len()
    )?;
    for (k, v) in &reply.headers {
        write!(out, "{k}: {v}\r\n")?;
    }
    out.write_all(b"\r\n")?;
    out.write_all(&reply.body)
}

fn mirror(tmp: &TempDir) -> Mirror {
    Mirror::open(MirrorConfig::new(tmp.path().join("m"))).unwrap()
}

/// Test fetch config: no pacing, fast retries.
fn fast_cfg(s: &StandIn) -> FetchConfig {
    FetchConfig {
        base_url: s.base_url.clone(),
        delay: Duration::ZERO,
        retries: 3,
        backoff: Duration::from_millis(5),
    }
}

#[test]
fn second_process_is_locked_out() {
    let tmp = TempDir::new().unwrap();
    let _first = mirror(&tmp);
    match Mirror::open(MirrorConfig::new(tmp.path().join("m"))) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("expected MirrorLocked, got {e}"),
        Ok(_) => panic!("second open of a live root must fail"),
    }
}

/// Read-side opens share the root among themselves and exclude (only)
/// the writer — in both directions; a read handle refuses to update.
#[test]
fn read_open_is_shared_and_read_only() {
    let tmp = TempDir::new().unwrap();
    drop(mirror(&tmp)); // create the root, release the writer lock
    let cfg = || MirrorConfig::new(tmp.path().join("m"));

    let r1 = Mirror::open_read(cfg()).unwrap();
    let r2 = Mirror::open_read(cfg()).unwrap();
    match Mirror::open(cfg()) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("writer must be excluded by readers, got {e}"),
        Ok(_) => panic!("writer must be excluded by readers"),
    }
    drop(r1);
    drop(r2);

    let w = mirror(&tmp);
    match Mirror::open_read(cfg()) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("reader must be excluded by the writer, got {e}"),
        Ok(_) => panic!("reader must be excluded by the writer"),
    }
    drop(w);

    let mut r = Mirror::open_read(cfg()).unwrap();
    let s = StandIn::start();
    match r.update(&Client::new(), &fast_cfg(&s), |_, _| ()) {
        Err(ietf_mirror::Error::ReadOnly) => {}
        other => panic!("read handle must refuse update, got {other:?}"),
    }

    // Never scaffold a store on the read path.
    match Mirror::open_read(MirrorConfig::new(tmp.path().join("no-such-mirror"))) {
        Err(ietf_mirror::Error::Io(_)) => {}
        Err(e) => panic!("read open of a non-mirror must fail loudly, got {e}"),
        Ok(_) => panic!("read open of a non-mirror must fail loudly"),
    }
    assert!(!tmp.path().join("no-such-mirror").exists());
}

/// The crash window the dirty flag exists for: kill the process AFTER
/// the store flush, BEFORE the watermark tx (a real abort, via the
/// IETFMAK_TEST_CRASH_AFTER_FLUSH knob, in a child `ietfmak update`).
/// The re-run must reconcile from the chain — zero re-fetches, aligned
/// watermarks, and above all NO duplicate revisions on the chain. Both
/// crash shapes are exercised: a fresh multi-revision import and a
/// single-revision head bump (the old `r < head` rebuild guard missed
/// the equal case, so exactly this shape used to duplicate).
#[test]
fn crash_between_flush_and_watermark_leaves_no_duplicates() {
    let s = StandIn::start();
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-crash-01\t2024-01-01\tActive\n"));
    s.text("draft-test-crash-00", "crash zero\n");
    s.text("draft-test-crash-01", "crash one\n");

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    let crashing_update = |base_url: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_ietfmak"))
            .args(["update", root.to_str().unwrap(), "--delay-ms", "0"])
            .env("IETFMAK_BASE_URL", base_url)
            .env("IETFMAK_TEST_CRASH_AFTER_FLUSH", "1")
            .output()
            .unwrap()
    };
    let no_dup_history = |m: &Mirror, want: &[&str]| {
        let revs: Vec<String> =
            m.history("draft-test-crash").unwrap().iter().map(|e| e.rev.clone()).collect();
        assert_eq!(revs, want, "history must be exact — duplicates are forever");
    };

    // Crash shape 1: fresh import (00+01 flushed, no watermark).
    let out = crashing_update(&s.base_url);
    assert!(!out.status.success(), "the crash knob must abort the child");
    assert_eq!(s.hits("/archive/id/draft-test-crash-00.txt"), 1);

    let content_before = s.content_hits().len();
    let mut m = mirror(&tmp);
    let st = m.update(&Client::new(), &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!(st.revisions_fetched, 0, "reconcile must not re-fetch flushed bytes");
    assert_eq!(st.revisions_reconciled, 2, "both crashed revisions aligned from the chain");
    assert_eq!(st.chains_rebuilt, 0, "alignment, not a rebuild");
    assert_eq!(s.content_hits().len(), content_before, "zero content GETs on reconcile");
    no_dup_history(&m, &["01", "00"]);
    drop(m);

    // Crash shape 2: single-revision head bump (the r == head case).
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-crash-02\t2024-02-01\tActive\n"));
    s.text("draft-test-crash-02", "crash two\n");
    let out = crashing_update(&s.base_url);
    assert!(!out.status.success(), "the crash knob must abort the child");
    assert_eq!(s.hits("/archive/id/draft-test-crash-02.txt"), 1, "02 fetched before the crash");

    let mut m = mirror(&tmp);
    let st = m.update(&Client::new(), &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!((st.revisions_fetched, st.revisions_reconciled), (0, 1));
    assert_eq!(s.hits("/archive/id/draft-test-crash-02.txt"), 1, "not re-fetched");
    no_dup_history(&m, &["02", "01", "00"]);
    assert_eq!(m.head("draft-test-crash").unwrap().unwrap().text, b"crash two\n");

    // The reconciled mirror is clean again: one more pass is a plain
    // idempotent no-op (no dirty flag left behind).
    let st = m.update(&Client::new(), &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!((st.revisions_fetched, st.revisions_reconciled), (0, 0));
    assert_eq!(st.revisions_skipped, 3);
    no_dup_history(&m, &["02", "01", "00"]);
}

#[test]
fn update_enumerates_history_from_latest_only_index() {
    let s = StandIn::start();
    // The REAL index shape: each draft appears ONCE, at its LATEST rev.
    s.route("/id/all_id.txt", |_| {
        Reply::ok(
            "# header comment\n\
             draft-test-alpha-03\t2024-04-01\tActive\n\
             draft-test-beta-00\t2024-03-01\tExpired\n\
             not-a-draft-line\n",
        )
    });
    s.text("draft-test-alpha-00", "alpha zero\n");
    s.text("draft-test-alpha-01", "alpha one\n");
    // draft-test-alpha-02 has no route → 404 (expired from the archive).
    s.text("draft-test-alpha-03", "alpha three\n");
    s.text("draft-test-beta-00", "beta zero\n");

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let client = Client::new();

    // Fresh import: the -03 listing fans out to 00,01,02,03.
    let st = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!(st.drafts_seen, 2);
    assert_eq!(st.drafts_new, 2);
    assert_eq!(st.revisions_fetched, 4, "alpha 00,01,03 + beta 00");
    assert_eq!(st.revisions_missing, 1, "alpha-02 404 → watermarked missing");
    assert_eq!(st.chains_rebuilt, 0);

    let head = m.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!(head.rev, "03");
    assert_eq!(head.text, b"alpha three\n");
    assert_eq!(head.date.as_deref(), Some("2024-04-01"), "index date pins the latest rev");
    let hist = m.history("draft-test-alpha").unwrap();
    assert_eq!(
        hist.iter().map(|e| e.rev.as_str()).collect::<Vec<_>>(),
        ["03", "01", "00"],
        "newest-first, the missing 02 absent"
    );
    assert_eq!(hist[2].text, b"alpha zero\n");
    assert_eq!(hist[1].date, None, "only the index's latest line carries a date");
    assert_eq!(m.head("draft-test-beta").unwrap().unwrap().text, b"beta zero\n");
    assert_eq!(m.drafts().unwrap().len(), 2);

    // Second run: idempotent — ZERO content GETs (including the 404).
    let before = s.content_hits().len();
    let st2 = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!(st2.revisions_fetched, 0);
    assert_eq!(st2.revisions_missing, 0, "404 watermarked, not re-tried");
    assert_eq!(st2.revisions_skipped, 5, "alpha 00..03 + beta 00");
    assert_eq!(s.content_hits().len(), before, "no content re-GET at all");

    // Head bump 03 → 05: ONLY 04 and 05 are fetched; 05 ends newest.
    s.route("/id/all_id.txt", |_| {
        Reply::ok(
            "draft-test-alpha-05\t2024-06-01\tActive\n\
             draft-test-beta-00\t2024-03-01\tExpired\n",
        )
    });
    s.text("draft-test-alpha-04", "alpha four\n");
    s.text("draft-test-alpha-05", "alpha five\n");
    let st3 = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!((st3.revisions_fetched, st3.drafts_new), (2, 0));
    assert_eq!(s.hits("/archive/id/draft-test-alpha-04.txt"), 1);
    assert_eq!(s.hits("/archive/id/draft-test-alpha-05.txt"), 1);
    assert_eq!(s.hits("/archive/id/draft-test-alpha-00.txt"), 1, "old revisions untouched");
    let head = m.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!((head.rev.as_str(), head.text.as_slice()), ("05", b"alpha five\n".as_slice()));
    assert_eq!(
        m.history("draft-test-alpha").unwrap().iter().map(|e| e.rev.clone()).collect::<Vec<_>>(),
        ["05", "04", "03", "01", "00"]
    );

    // Durability: a fresh open over the same root serves the same data.
    drop(m);
    let m2 = mirror(&tmp);
    assert_eq!(m2.head("draft-test-alpha").unwrap().unwrap().rev, "05");
    assert_eq!(m2.history("draft-test-alpha").unwrap().len(), 5);
    assert_eq!(m2.head("draft-test-beta").unwrap().unwrap().text, b"beta zero\n");
}

#[test]
fn transient_500_is_retried_to_success() {
    let s = StandIn::start();
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-flaky-00\t2024-01-01\tActive\n"));
    let mut calls = 0u32;
    s.route("/archive/id/draft-test-flaky-00.txt", move |_| {
        calls += 1;
        if calls == 1 { Reply::status(500) } else { Reply::ok("flaky zero\n") }
    });

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let st = m.update(&Client::new(), &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!(st.revisions_fetched, 1);
    assert_eq!(s.hits("/archive/id/draft-test-flaky-00.txt"), 2, "one 500, one 200");
    assert_eq!(m.head("draft-test-flaky").unwrap().unwrap().text, b"flaky zero\n");
}

#[test]
fn persistent_500_fails_loud_and_rerun_resumes() {
    let s = StandIn::start();
    // The index carries an ETag and honors If-None-Match — so this also
    // proves validators are NOT stored by a failed pass.
    s.route("/id/all_id.txt", |hit| {
        if hit.if_none_match.as_deref() == Some("\"g1\"") {
            Reply::status(304)
        } else {
            Reply::ok("draft-test-gamma-01\t2024-02-01\tActive\n").header("ETag", "\"g1\"")
        }
    });
    s.text("draft-test-gamma-00", "gamma zero\n");
    let broken = Arc::new(AtomicBool::new(true));
    let b = broken.clone();
    s.route("/archive/id/draft-test-gamma-01.txt", move |_| {
        if b.load(Ordering::SeqCst) { Reply::status(500) } else { Reply::ok("gamma one\n") }
    });

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let client = Client::new();
    let cfg = FetchConfig { retries: 2, ..fast_cfg(&s) };

    match m.update(&client, &cfg, |_, _| ()) {
        Err(ietf_mirror::Error::HttpStatus { status: 500, .. }) => {}
        other => panic!("expected loud 500 failure, got {other:?}"),
    }
    assert_eq!(s.hits("/archive/id/draft-test-gamma-01.txt"), 3, "1 try + 2 retries");
    // Nothing corrupted: no layers landed, no watermarks written.
    assert!(m.head("draft-test-gamma").unwrap().is_none());

    // Re-run after the server recovers: the FULL index is re-fetched
    // (no 304 past unfinished work) and the draft completes.
    broken.store(false, Ordering::SeqCst);
    let st = m.update(&client, &cfg, |_, _| ()).unwrap();
    assert!(!st.index_not_modified, "failed pass must not have stored validators");
    assert_eq!(st.revisions_fetched, 2, "00 refetched (never watermarked) + 01");
    assert_eq!(
        m.history("draft-test-gamma").unwrap().iter().map(|e| e.rev.clone()).collect::<Vec<_>>(),
        ["01", "00"]
    );

    // NOW the validators are stored: a third run is a 304 short-circuit.
    let st = m.update(&client, &cfg, |_, _| ()).unwrap();
    assert!(st.index_not_modified);
}

#[test]
fn index_304_short_circuits() {
    let s = StandIn::start();
    s.route("/id/all_id.txt", |hit| {
        if hit.if_none_match.as_deref() == Some("\"v7\"") {
            Reply::status(304)
        } else {
            Reply::ok("draft-test-idx-00\t2024-01-01\tActive\n").header("ETag", "\"v7\"")
        }
    });
    s.text("draft-test-idx-00", "idx zero\n");

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let client = Client::new();

    let st = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
    assert!(!st.index_not_modified);
    assert_eq!(st.revisions_fetched, 1);

    let content_before = s.content_hits().len();
    let st2 = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
    assert!(st2.index_not_modified, "unchanged index answers 304");
    assert_eq!((st2.drafts_seen, st2.revisions_fetched, st2.revisions_skipped), (0, 0, 0));
    assert_eq!(s.content_hits().len(), content_before, "304 pass makes no content GETs");
    assert_eq!(s.hits("/id/all_id.txt"), 2);
    let second = s.last_hit("/id/all_id.txt").unwrap();
    assert_eq!(second.if_none_match.as_deref(), Some("\"v7\""), "validator was sent");
}

#[test]
fn pacing_spreads_content_gets() {
    let s = StandIn::start();
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-paced-03\t2024-01-01\tActive\n"));
    for (doc, body) in [
        ("draft-test-paced-00", "p0\n"),
        ("draft-test-paced-01", "p1\n"),
        ("draft-test-paced-02", "p2\n"),
        ("draft-test-paced-03", "p3\n"),
    ] {
        s.text(doc, body);
    }

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let delay = Duration::from_millis(120);
    let cfg = FetchConfig { delay, ..fast_cfg(&s) };
    m.update(&Client::new(), &cfg, |_, _| ()).unwrap();

    let hits = s.content_hits();
    assert_eq!(hits.len(), 4);
    let span = hits.last().unwrap().at.duration_since(hits.first().unwrap().at);
    assert!(
        span >= delay * (hits.len() as u32 - 1),
        "4 content GETs must span >= 3 delays, spanned {span:?}"
    );
}

#[test]
fn legacy_heads_only_mirror_backfills_in_order() {
    let s = StandIn::start();
    // Manufacture the legacy heads-only shape via public machinery: an
    // update where everything below the head is (temporarily) 404 gives
    // a singleton chain at -05; stripping the `missing` watermarks then
    // leaves exactly what the old fantasy-index code left behind — a
    // newer head with its older revisions unseen.
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-delta-05\t2024-05-01\tActive\n"));
    s.text("draft-test-delta-05", "delta five\n");

    let tmp = TempDir::new().unwrap();
    let client = Client::new();
    {
        let mut m = mirror(&tmp);
        let st = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
        assert_eq!((st.revisions_fetched, st.revisions_missing), (1, 5));
    }
    let conn = rusqlite::Connection::open(tmp.path().join("m/meta.db")).unwrap();
    conn.execute("DELETE FROM revisions_seen WHERE missing = 1", []).unwrap();
    drop(conn);

    // The archive serves the older revisions now, and the head bumps.
    for (doc, body) in [
        ("draft-test-delta-00", "delta zero\n"),
        ("draft-test-delta-01", "delta one\n"),
        ("draft-test-delta-02", "delta two\n"),
        ("draft-test-delta-03", "delta three\n"),
        ("draft-test-delta-04", "delta four\n"),
        ("draft-test-delta-06", "delta six\n"),
    ] {
        s.text(doc, body);
    }
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-delta-06\t2024-06-01\tActive\n"));

    let mut m = mirror(&tmp);
    let st = m.update(&client, &fast_cfg(&s), |_, _| ()).unwrap();
    assert_eq!(st.revisions_fetched, 6, "00..04 backfilled + 06");
    assert_eq!(st.chains_rebuilt, 1, "backfill under an existing head rebuilds the chain");
    assert_eq!(s.hits("/archive/id/draft-test-delta-05.txt"), 1, "already-mirrored rev not re-GET");

    let hist = m.history("draft-test-delta").unwrap();
    assert_eq!(
        hist.iter().map(|e| e.rev.as_str()).collect::<Vec<_>>(),
        ["06", "05", "04", "03", "02", "01", "00"],
        "rebuilt chain is newest-first across old and backfilled revisions"
    );
    assert_eq!(hist[1].text, b"delta five\n", "pre-rebuild bytes preserved");
    assert_eq!(m.head("draft-test-delta").unwrap().unwrap().rev, "06");

    // Durability across the repoint.
    drop(m);
    let m2 = mirror(&tmp);
    assert_eq!(m2.history("draft-test-delta").unwrap().len(), 7);
    assert_eq!(m2.head("draft-test-delta").unwrap().unwrap().text, b"delta six\n");
}
