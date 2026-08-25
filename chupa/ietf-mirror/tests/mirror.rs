//! End-to-end acceptance for the IETF-drafts mirror against a scripted
//! stand-in for www.ietf.org speaking the REAL `all_id.txt` shape.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ietf_mirror::{FetchConfig, Mirror, MirrorConfig};
use reqwest::blocking::Client;
use tempfile::TempDir;

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

#[derive(Clone)]
struct Hit {
    path: String,
    if_none_match: Option<String>,
    at: Instant,
}

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

    fn text(&self, docname: &str, body: &'static str) {
        self.route(&format!("/archive/id/{docname}.txt"), move |_| Reply::ok(body));
    }

    fn hits(&self, path: &str) -> usize {
        self.log.lock().unwrap().iter().filter(|h| h.path == path).count()
    }

    fn content_hits(&self) -> Vec<Hit> {
        self.log.lock().unwrap().iter().filter(|h| h.path.starts_with("/archive/")).cloned().collect()
    }

    fn last_hit(&self, path: &str) -> Option<Hit> {
        self.log.lock().unwrap().iter().rev().find(|h| h.path == path).cloned()
    }
}

fn serve_one(stream: TcpStream, routes: &Mutex<HashMap<String, Handler>>, log: &Mutex<Vec<Hit>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut if_none_match = None;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        let h = h.trim_end();
        if h.is_empty() { break; }
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
        200 => "OK", 304 => "Not Modified", 404 => "Not Found", 500 => "Internal Server Error",
        _ => "Status",
    };
    let mut out = stream;
    write!(out, "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: {}\r\n", reply.status, reason, reply.body.len())?;
    for (k, v) in &reply.headers { write!(out, "{k}: {v}\r\n")?; }
    out.write_all(b"\r\n")?;
    out.write_all(&reply.body)
}

fn mirror(tmp: &TempDir) -> Mirror {
    Mirror::open(MirrorConfig::new(tmp.path().join("m"))).unwrap()
}

fn fast_cfg(s: &StandIn) -> FetchConfig {
    FetchConfig { base_url: s.base_url.clone(), delay: Duration::ZERO, retries: 3, backoff: Duration::from_millis(5) }
}

#[test]
fn second_process_is_locked_out() {
    let tmp = TempDir::new().unwrap();
    let _first = mirror(&tmp);
    match Mirror::open(MirrorConfig::new(tmp.path().join("m"))) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("expected MirrorLocked, got {e}"),
        Ok(_) => panic!("second open must fail"),
    }
}

#[test]
fn read_open_is_shared_and_read_only() {
    let tmp = TempDir::new().unwrap();
    drop(mirror(&tmp));
    let cfg = || MirrorConfig::new(tmp.path().join("m"));
    let r1 = Mirror::open_read(cfg()).unwrap();
    let r2 = Mirror::open_read(cfg()).unwrap();
    match Mirror::open(cfg()) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("writer excluded by readers: {e}"),
        Ok(_) => panic!("writer excluded by readers"),
    }
    drop(r1);
    drop(r2);
    let w = mirror(&tmp);
    match Mirror::open_read(cfg()) {
        Err(ietf_mirror::Error::MirrorLocked(_)) => {}
        Err(e) => panic!("reader excluded by writer: {e}"),
        Ok(_) => panic!("reader excluded by writer"),
    }
    drop(w);
    let mut r = Mirror::open_read(cfg()).unwrap();
    let s = StandIn::start();
    match r.update(&Client::new(), &fast_cfg(&s), |_| ()) {
        Err(ietf_mirror::Error::ReadOnly) => {}
        other => panic!("read handle must refuse update: {other:?}"),
    }
    match Mirror::open_read(MirrorConfig::new(tmp.path().join("no-such"))) {
        Err(ietf_mirror::Error::Io(_)) => {}
        Err(e) => panic!("read open of non-mirror: {e}"),
        Ok(_) => panic!("read open of non-mirror must fail"),
    }
    assert!(!tmp.path().join("no-such").exists());
}

#[test]
fn update_enumerates_history_from_latest_only_index() {
    let s = StandIn::start();
    s.route("/id/all_id.txt", |_| {
        Reply::ok("# header\n\
                   draft-test-alpha-03\t2024-04-01\tActive\n\
                   draft-test-beta-00\t2024-03-01\tExpired\n\
                   not-a-draft-line\n")
    });
    s.text("draft-test-alpha-00", "alpha zero\n");
    s.text("draft-test-alpha-01", "alpha one\n");
    s.text("draft-test-alpha-03", "alpha three\n");
    s.text("draft-test-beta-00", "beta zero\n");

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    let client = Client::new();

    let st = m.update(&client, &fast_cfg(&s), |_| ()).unwrap();
    assert_eq!(st.drafts_seen, 2);
    assert_eq!(st.revisions_fetched, 4);
    assert_eq!(st.revisions_missing, 1, "alpha-02 404");

    let head = m.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!(head.rev, "03");
    assert_eq!(head.text, b"alpha three\n");
    assert_eq!(head.date.as_deref(), Some("2024-04-01"));
    let hist = m.history("draft-test-alpha").unwrap();
    assert_eq!(
        hist.iter().map(|e| e.rev.as_str()).collect::<Vec<_>>(),
        ["03", "01", "00"]
    );
    assert_eq!(hist[2].text, b"alpha zero\n");
    assert_eq!(hist[1].date, None);
    assert_eq!(m.head("draft-test-beta").unwrap().unwrap().text, b"beta zero\n");
    assert_eq!(m.drafts().unwrap().len(), 2);

    // Idempotent second run
    let before = s.content_hits().len();
    let st2 = m.update(&client, &fast_cfg(&s), |_| ()).unwrap();
    assert_eq!(st2.revisions_fetched, 0);
    assert_eq!(st2.revisions_missing, 0);
    assert_eq!(st2.revisions_skipped, 5);
    assert_eq!(s.content_hits().len(), before);

    // Head bump 03 → 05
    s.route("/id/all_id.txt", |_| {
        Reply::ok("draft-test-alpha-05\t2024-06-01\tActive\n\
                   draft-test-beta-00\t2024-03-01\tExpired\n")
    });
    s.text("draft-test-alpha-04", "alpha four\n");
    s.text("draft-test-alpha-05", "alpha five\n");
    let st3 = m.update(&client, &fast_cfg(&s), |_| ()).unwrap();
    assert_eq!(st3.revisions_fetched, 2);
    assert_eq!(s.hits("/archive/id/draft-test-alpha-04.txt"), 1);
    assert_eq!(s.hits("/archive/id/draft-test-alpha-05.txt"), 1);
    let head = m.head("draft-test-alpha").unwrap().unwrap();
    assert_eq!((head.rev.as_str(), head.text.as_slice()), ("05", b"alpha five\n".as_slice()));
    assert_eq!(
        m.history("draft-test-alpha").unwrap().iter().map(|e| e.rev.clone()).collect::<Vec<_>>(),
        ["05", "04", "03", "01", "00"]
    );

    // Durability: fresh open
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
    let st = m.update(&Client::new(), &fast_cfg(&s), |_| ()).unwrap();
    assert_eq!(st.revisions_fetched, 1);
    assert_eq!(s.hits("/archive/id/draft-test-flaky-00.txt"), 2);
    assert_eq!(m.head("draft-test-flaky").unwrap().unwrap().text, b"flaky zero\n");
}

#[test]
fn persistent_500_fails_loud_and_rerun_resumes() {
    let s = StandIn::start();
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

    match m.update(&client, &cfg, |_| ()) {
        Err(ietf_mirror::Error::HttpStatus { status: 500, .. }) => {}
        other => panic!("expected 500, got {other:?}"),
    }
    assert_eq!(s.hits("/archive/id/draft-test-gamma-01.txt"), 3);
    assert!(m.head("draft-test-gamma").unwrap().is_none());

    broken.store(false, Ordering::SeqCst);
    let st = m.update(&client, &cfg, |_| ()).unwrap();
    assert!(!st.index_not_modified, "failed pass must not store validators");
    assert_eq!(st.revisions_fetched, 2, "00 refetched + 01");
    assert_eq!(
        m.history("draft-test-gamma").unwrap().iter().map(|e| e.rev.clone()).collect::<Vec<_>>(),
        ["01", "00"]
    );

    let st = m.update(&client, &cfg, |_| ()).unwrap();
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

    let st = m.update(&client, &fast_cfg(&s), |_| ()).unwrap();
    assert!(!st.index_not_modified, "first pass downloads");
    assert_eq!(st.revisions_fetched, 1);

    let content_before = s.content_hits().len();
    let st2 = m.update(&client, &fast_cfg(&s), |_| ()).unwrap();
    assert!(st2.index_not_modified, "unchanged index answers 304");
    assert_eq!(s.content_hits().len(), content_before);
    let second = s.last_hit("/id/all_id.txt").unwrap();
    assert_eq!(second.if_none_match.as_deref(), Some("\"v7\""));
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
    m.update(&Client::new(), &cfg, |_| ()).unwrap();

    let hits = s.content_hits();
    assert_eq!(hits.len(), 4);
    let span = hits.last().unwrap().at.duration_since(hits.first().unwrap().at);
    assert!(span >= delay * 3, "4 GETs must span >= 3 delays, spanned {span:?}");
}

#[test]
fn revision_pinned_read() {
    let s = StandIn::start();
    s.route("/id/all_id.txt", |_| Reply::ok("draft-test-pin-01\t2024-02-01\tActive\n"));
    s.text("draft-test-pin-00", "pin zero\n");
    s.text("draft-test-pin-01", "pin one\n");

    let tmp = TempDir::new().unwrap();
    let mut m = mirror(&tmp);
    m.update(&Client::new(), &fast_cfg(&s), |_| ()).unwrap();

    let r00 = m.revision("draft-test-pin", "00").unwrap().unwrap();
    assert_eq!(r00.text, b"pin zero\n");
    assert_eq!(r00.rev, "00");

    let r01 = m.revision("draft-test-pin", "01").unwrap().unwrap();
    assert_eq!(r01.text, b"pin one\n");

    assert!(m.revision("draft-test-pin", "99").unwrap().is_none());
    assert!(m.revision("draft-no-such", "00").unwrap().is_none());
}
