//! BUG 3: the CLI must not panic ("failed printing to stdout: Broken
//! pipe") when its streamed output is piped into a reader that closes
//! early (`wikimak history … | head`). The standalone binary resets
//! SIGPIPE to SIG_DFL, so a closed downstream pipe terminates the process
//! with the signal (exit 141) instead of the EPIPE the `println!` macros
//! turn into a panic.

mod common;

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

use common::make_instance;
use wikimak_mediawiki::new_page_stream;

/// A one-page dump with `revs` revisions — enough history that `history`
/// output dwarfs the pipe buffer, so the child is still writing when the
/// downstream reader closes.
fn big_dump(page_id: u64, revs: usize) -> Vec<u8> {
    let mut body = String::new();
    for i in 0..revs {
        let id = 1000 + i as u64;
        let (h, m, s) = ((i / 3600) % 24, (i / 60) % 60, i % 60);
        body.push_str(&format!(
            "<revision><id>{id}</id><timestamp>2024-01-01T{h:02}:{m:02}:{s:02}Z</timestamp>\
             <contributor><username>A</username><id>1</id></contributor>\
             <comment>c</comment><model>wikitext</model><format>text/x-wiki</format>\
             <text bytes=\"3\" xml:space=\"preserve\">txt</text><sha1></sha1></revision>"
        ));
    }
    format!(
        "<mediawiki xmlns=\"http://www.mediawiki.org/xml/export-0.11/\" version=\"0.11\" \
         xml:lang=\"en\"><siteinfo><sitename>T</sitename><dbname>testwiki</dbname>\
         <namespaces><namespace key=\"0\" case=\"first-letter\" /></namespaces></siteinfo>\
         <page><title>P</title><ns>0</ns><id>{page_id}</id>{body}</page></mediawiki>"
    )
    .into_bytes()
}

#[test]
fn history_piped_to_early_close_does_not_panic() {
    const PAGE: u64 = 4242;
    let tmp = tempfile::tempdir().unwrap();
    {
        let inst = make_instance(&tmp, 1 << 20);
        let mut s = new_page_stream(std::io::Cursor::new(big_dump(PAGE, 3000)));
        inst.import(&mut s).expect("import");
        inst.flush().unwrap();
    } // drop → release the exclusive flock so the CLI can open the root

    let mut child = Command::new(env!("CARGO_BIN_EXE_wikimak"))
        .args(["history", tmp.path().to_str().unwrap(), &PAGE.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wikimak");

    // Read ONE line, then close the read end — the child keeps writing
    // into a now-closed pipe (the `| head -1` shape).
    {
        let out = child.stdout.take().unwrap();
        let mut r = BufReader::new(out);
        let mut line = String::new();
        let _ = r.read_line(&mut line);
        assert!(line.starts_with("rev "), "first history line, got {line:?}");
    } // BufReader dropped → pipe read end closed

    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    let status = child.wait().unwrap();

    // Never a panic: a Broken-pipe panic exits 101 and prints "panicked".
    assert_ne!(status.code(), Some(101), "history panicked on the closed pipe: {stderr}");
    assert!(
        !stderr.contains("panicked") && !stderr.contains("failed printing to stdout"),
        "history printed a panic message: {stderr}"
    );
    // With SIGPIPE reset to default the child is terminated by the signal
    // (13); a fully-buffered small run could instead exit 0. Both are
    // acceptable — neither is a panic.
    assert!(
        status.signal() == Some(13) || status.success(),
        "expected SIGPIPE termination or success, got {status:?} (stderr: {stderr})"
    );
}
