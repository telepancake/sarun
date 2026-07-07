//! End-to-end serve proof (browsing plan §5, §6 date-picker correctness).
//!
//! Run: `cargo test -p wikimak-wikipedia --test serve_e2e` (needs the
//! default `serve` feature). Imports a synthesized wiki — a `Page` that
//! transcludes `Template:Infobox` and `#invoke`s `Module:Greet`, with TWO
//! revisions of both the page AND the template at different timestamps —
//! starts the real server on an ephemeral port, and drives it with plain
//! `std::net::TcpStream` HTTP GETs.
//!
//! The load-bearing assertion is the wayback one (b): at a τ between the
//! two revisions, the OLD page text renders against the OLD template
//! revision. That exercises template-revision-at-τ end to end — the whole
//! point of the local renderer (plan §1).

// This suite drives the serve stack (renderer + HTTP), so it only builds
// with the `serve` feature — matching its sibling `tau_render.rs`. Without
// this guard a `--no-default-features --features fetch` build fails on the
// `wikimak_wikipedia::serve` import.
#![cfg(feature = "serve")]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use wikimak_mediawiki::new_page_stream;

use common::make_instance;
use wikimak_wikipedia::serve::{serve, ServeConfig};

/// Two revisions of `Page` (Alpha→Beta) and of `Template:Infobox`
/// (Infobox OLD→Infobox NEW) at 2020-01-01 and 2022-01-01; one revision of
/// `Module:Greet`. A red link to a page that does not exist.
const FIXTURE: &str = r#"<mediawiki xmlns="http://www.mediawiki.org/xml/export-0.11/" version="0.11" xml:lang="en">
  <siteinfo>
    <sitename>Serve Test Wiki</sitename><dbname>servetestwiki</dbname>
    <base>http://serve.test/wiki/Main_Page</base>
    <generator>g</generator><case>first-letter</case>
    <namespaces>
      <namespace key="0" case="first-letter"/>
      <namespace key="6" case="first-letter">File</namespace>
      <namespace key="10" case="first-letter">Template</namespace>
      <namespace key="14" case="first-letter">Category</namespace>
      <namespace key="828" case="first-letter">Module</namespace>
    </namespaces>
  </siteinfo>
  <page>
    <title>Page</title><ns>0</ns><id>100</id>
    <revision>
      <id>101</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>old</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{{Infobox|name=Alpha}}
Greeting: {{#invoke:Greet|hello|Alpha}}
[[Nonexistent Page]]</text><sha1>p1</sha1>
    </revision>
    <revision>
      <id>102</id><parentid>101</parentid><timestamp>2022-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>new</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{{Infobox|name=Beta}}
Greeting: {{#invoke:Greet|hello|Beta}}
[[Nonexistent Page]]</text><sha1>p2</sha1>
    </revision>
  </page>
  <page>
    <title>Template:Infobox</title><ns>10</ns><id>200</id>
    <revision>
      <id>201</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>old tpl</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{| class="infobox"
! Infobox OLD
|-
| Name: {{{name}}}
|}</text><sha1>t1</sha1>
    </revision>
    <revision>
      <id>202</id><parentid>201</parentid><timestamp>2022-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>new tpl</comment><model>wikitext</model><format>text/x-wiki</format>
      <text xml:space="preserve">{| class="infobox"
! Infobox NEW
|-
| Name: {{{name}}}
|}</text><sha1>t2</sha1>
    </revision>
  </page>
  <page>
    <title>Module:Greet</title><ns>828</ns><id>300</id>
    <revision>
      <id>301</id><timestamp>2020-01-01T00:00:00Z</timestamp>
      <contributor><username>Ed</username><id>1</id></contributor>
      <comment>mod</comment><model>Scribunto</model><format>text/plain</format>
      <text xml:space="preserve">local p = {}
function p.hello(frame)
  return "Hello " .. (frame.args[1] or "world")
end
return p</text><sha1>m1</sha1>
    </revision>
  </page>
</mediawiki>"#;

/// Grab a free port by binding :0, then release it for the server to claim.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().unwrap().port()
}

/// A minimal HTTP/1.1 GET over a raw socket. Returns (status_code, body).
fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n",
        path = path,
        addr = addr,
    );
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .expect("status line");
    let body = match text.split_once("\r\n\r\n") {
        Some((_, b)) => b.to_string(),
        None => String::new(),
    };
    (status, body)
}

/// Wait until the server accepts connections (or panic after a timeout).
fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("server never became ready at {addr}");
}

#[test]
fn serve_renders_and_waybacks_a_synthesized_wiki() {
    let tmp = TempDir::new().unwrap();
    let inst = make_instance(&tmp, 4096);
    let mut stream = new_page_stream(std::io::Cursor::new(FIXTURE.as_bytes().to_vec()));
    inst.import(&mut stream).expect("import fixture");
    inst.flush().expect("flush");

    // τ strictly between the two page revisions (2020 and 2022), computed
    // from the real stored timestamps so the between-ness is exact.
    let mut times: Vec<i64> = inst
        .page_history(100)
        .expect("history")
        .map(|e| e.expect("entry").meta.ts.timestamp_micros())
        .collect();
    times.sort();
    assert_eq!(times.len(), 2, "page has two revisions");
    let tau_between = (times[0] + times[1]) / 2;

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = ServeConfig {
        addr: addr.clone(),
        media_cache: tmp.path().join("media"),
    };
    // serve() blocks forever; run it on a detached thread for the test's
    // lifetime.
    thread::spawn(move || {
        let _ = serve(inst, cfg);
    });
    wait_ready(&addr);

    // (a) current /wiki/Page: CURRENT template revision + module output.
    let (status, body) = http_get(&addr, "/wiki/Page");
    assert_eq!(status, 200, "current page status");
    assert!(body.contains("<table class=\"infobox\">"), "infobox table html present:\n{body}");
    assert!(body.contains("Infobox NEW"), "current template revision rendered:\n{body}");
    assert!(body.contains("Name: Beta"), "current page text (Beta) rendered:\n{body}");
    assert!(body.contains("Hello Beta"), "module output for current text:\n{body}");
    assert!(!body.contains("Infobox OLD"), "no old template content at head:\n{body}");

    // (c) the red link carries class="new".
    assert!(body.contains("class=\"new\""), "red link marked new:\n{body}");

    // (b) THE WAYBACK ASSERTION: at τ between the revisions, OLD page text
    // renders against the OLD template revision (and the OLD module arg).
    let (status, body) = http_get(&addr, &format!("/wiki/Page?asof={tau_between}"));
    assert_eq!(status, 200, "asof page status");
    assert!(body.contains("Infobox OLD"), "OLD template revision at τ:\n{body}");
    assert!(body.contains("Name: Alpha"), "OLD page text (Alpha) at τ:\n{body}");
    assert!(body.contains("Hello Alpha"), "module output for old text at τ:\n{body}");
    assert!(!body.contains("Infobox NEW"), "no new template content at τ:\n{body}");
    assert!(!body.contains("Name: Beta"), "no new page text at τ:\n{body}");
    // asof propagates into internal links.
    assert!(
        body.contains(&format!("?asof={tau_between}")),
        "asof carried through internal links:\n{body}"
    );

    // A date-form asof between the revisions renders the same wayback view.
    let (status, body) = http_get(&addr, "/wiki/Page?asof=2021-01-01");
    assert_eq!(status, 200);
    assert!(body.contains("Infobox OLD"), "date-form asof waybacks too:\n{body}");
    assert!(body.contains("Name: Alpha"), "date-form asof old text:\n{body}");

    // (d) history lists BOTH revisions.
    let (status, body) = http_get(&addr, "/w/history/Page");
    assert_eq!(status, 200, "history status");
    assert!(body.contains("rev 101"), "history lists rev 101:\n{body}");
    assert!(body.contains("rev 102"), "history lists rev 102:\n{body}");

    // allpages lists the mainspace article; `/` redirects there.
    let (status, body) = http_get(&addr, "/w/allpages");
    assert_eq!(status, 200);
    assert!(body.contains("/wiki/Page"), "allpages links the article:\n{body}");

    let (status, _body) = http_get(&addr, "/");
    assert_eq!(status, 302, "root redirects to allpages");
}
