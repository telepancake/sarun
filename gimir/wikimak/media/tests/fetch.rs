//! Fetch-path integration tests (feature `fetch`).
//!
//! These drive [`MediaStore::materialize`] against a hand-rolled
//! localhost HTTP server (no test-only network deps, no crate deps added
//! to the manifest) so the assertions pin REAL behavior: the exact URL
//! path the store requests, that 200 bytes land in the blob store, that
//! a 404 is negative-cached (no re-request), and that a 5xx arms the
//! Robot-policy cooldown that then short-circuits later fetches.
//!
//! Run: `cargo test -p wikimak-media --features fetch`.
#![cfg(feature = "fetch")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use wikimak_media::{MediaError, MediaStore, Repo};

/// How the mock server should respond, per test.
#[derive(Clone, Copy)]
enum Mode {
    Ok,
    NotFound,
    ServerError,
}

struct Mock {
    /// Requested paths, in order (proves what the store asked for).
    paths: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicUsize>,
    base: String,
}

fn start_mock(mode: Mode, body: &'static [u8]) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let paths = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let paths_t = paths.clone();
    let hits_t = hits.clone();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            hits_t.fetch_add(1, Ordering::SeqCst);
            handle(stream, mode, body, &paths_t);
        }
    });

    Mock {
        paths,
        hits,
        base: format!("http://127.0.0.1:{port}/wikipedia/commons"),
    }
}

fn handle(mut stream: TcpStream, mode: Mode, body: &[u8], paths: &Arc<Mutex<Vec<String>>>) {
    // Read just the request line + headers (we never send a request body).
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    if let Some(first) = head.lines().next() {
        // "GET /path HTTP/1.1"
        if let Some(path) = first.split_whitespace().nth(1) {
            paths.lock().unwrap().push(path.to_string());
        }
    }
    let resp = match mode {
        Mode::Ok => {
            let mut r = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            r.extend_from_slice(body);
            r
        }
        Mode::NotFound => {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        }
        Mode::ServerError => {
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec()
        }
    };
    let _ = stream.write_all(&resp);
    let _ = stream.flush();
}

fn tmp_root(tag: &str) -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "wikimak-media-fetch-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

#[test]
fn fetch_success_stores_thumb_and_requests_correct_path() {
    let mock = start_mock(Mode::Ok, b"\x89PNG-bytes");
    let root = tmp_root("ok");
    let ms = MediaStore::new(&root, vec![Repo::new("commons", &mock.base)]);

    // 120px thumb of Example.jpg → /thumb/a/a9/Example.jpg/120px-Example.jpg.
    let path = ms.materialize("Example.jpg", Some(120)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG-bytes");

    let requested = mock.paths.lock().unwrap().clone();
    assert_eq!(
        requested,
        vec!["/wikipedia/commons/thumb/a/a9/Example.jpg/120px-Example.jpg".to_string()],
        "store must request the VERIFIED thumb URL path"
    );

    // Second call is a cache hit — no further request.
    let again = ms.materialize("Example.jpg", Some(120)).unwrap();
    assert_eq!(again, path);
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1, "cache hit must not refetch");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn fetch_original_requests_unscaled_path() {
    let mock = start_mock(Mode::Ok, b"orig");
    let root = tmp_root("orig");
    let ms = MediaStore::new(&root, vec![Repo::new("commons", &mock.base)]);

    ms.materialize("Example.jpg", None).unwrap();
    let requested = mock.paths.lock().unwrap().clone();
    assert_eq!(
        requested,
        vec!["/wikipedia/commons/a/a9/Example.jpg".to_string()],
        "None width must fetch the original (no /thumb/)"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn fetch_404_negative_caches_and_does_not_refetch() {
    let mock = start_mock(Mode::NotFound, b"");
    let root = tmp_root("404");
    let ms = MediaStore::new(&root, vec![Repo::new("commons", &mock.base)]);

    let err = ms.materialize("Ghost.png", Some(60)).unwrap_err();
    assert!(matches!(err, MediaError::NotFound(_)), "got {err:?}");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);

    // The negative entry means the second call answers offline-locally.
    let err2 = ms.materialize("Ghost.png", Some(60)).unwrap_err();
    assert!(matches!(err2, MediaError::NotFound(_)), "got {err2:?}");
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        1,
        "a negative-cached 404 must NOT hit the network again"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn fetch_5xx_arms_cooldown_that_short_circuits() {
    let mock = start_mock(Mode::ServerError, b"");
    let root = tmp_root("5xx");
    let ms = MediaStore::new(&root, vec![Repo::new("commons", &mock.base)]);

    let err = ms.materialize("Flaky.jpg", Some(120)).unwrap_err();
    assert!(matches!(err, MediaError::Backoff(_)), "got {err:?}");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);

    // Cooldown is armed: a fetch for a DIFFERENT file backs off without
    // touching the network (Robot policy 5xx pause).
    let err2 = ms.materialize("Other.jpg", Some(120)).unwrap_err();
    assert!(matches!(err2, MediaError::Backoff(_)), "got {err2:?}");
    assert_eq!(
        mock.hits.load(Ordering::SeqCst),
        1,
        "cooldown must suppress further requests"
    );
    // 5xx is NOT negative-cached (transient) — no sentinel written.
    assert!(!ms
        .blobs()
        .has_negative("Flaky.jpg", wikimak_media::Bucket::Px(120)));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn repo_chain_falls_through_404_to_next_repo() {
    // First repo 404s everything; second serves the bytes. The store
    // must try repo 1, get 404, then succeed at repo 2.
    let miss = start_mock(Mode::NotFound, b"");
    let hit = start_mock(Mode::Ok, b"from-commons");
    let root = tmp_root("chain");
    let ms = MediaStore::new(
        &root,
        vec![
            Repo::new("local", &miss.base),
            Repo::new("commons", &hit.base),
        ],
    );

    let path = ms.materialize("Example.jpg", Some(250)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"from-commons");
    assert_eq!(miss.hits.load(Ordering::SeqCst), 1, "local repo tried first");
    assert_eq!(hit.hits.load(Ordering::SeqCst), 1, "commons served the miss");
    std::fs::remove_dir_all(&root).ok();
}
