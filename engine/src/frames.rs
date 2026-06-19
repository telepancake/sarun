// Muxed box-channel framing — the ONE box↔engine connection (mirrors the Python
// engine's frame protocol byte-for-byte so a Python UI/runner and the Rust ones
// interoperate over the same socket).
//
// After the register handshake, the box runner's single connection carries
// length-prefixed typed frames so fd-passing and byte streams coexist on one
// stream:
//
//     [4-byte big-endian total-len][1-byte type][payload]
//
// total-len counts the type byte + payload (not the 4-byte prefix). Frame types:
//   ECHO      (engine→inner) payload = [1-byte stream][bytes]; replayed to
//                            --inner's real fd 1/2 for live, upward visibility.
//   ECHO_DONE (engine→inner) empty; all captured bytes have been framed — inner
//                            may now stop reading and close the connection.
//   MUTE      (inner→engine) the sendmsg attaches SCM_RIGHTS [pidfd] of --inner
//                            ITSELF; the engine resolves --inner's HOST pid and
//                            adds it to a global muted set: writes by that pid
//                            are echoed but NOT recorded (so a nested box's echo
//                            readback travelling up through an ancestor sink is
//                            never re-captured).
//   UNMUTE    (inner→engine) empty; removes --inner's pid from the muted set.

pub const FRAME_ECHO: u8 = 2;
pub const FRAME_ECHO_DONE: u8 = 3;
pub const FRAME_MUTE: u8 = 4;
pub const FRAME_UNMUTE: u8 = 5;
//   PROV      (inner→engine) D9 semantic-provenance for the embedded brush
//             shell (-b). payload = a UTF-8 JSON object describing ONE shell
//             command brush is about to run: the exact command string plus the
//             pipeline/redirect structure brush actually parsed (NOT a Makefile
//             line — see D9). The engine records it into the box's sqlar
//             `brushprov` table and broadcasts a `brush_prov` event.
pub const FRAME_PROV: u8 = 6;
//
// Engine-held-PTY mux frames (D7/D9). On a `pty_spawn` control connection the
// engine spawns a command on a PTY it holds (portable-pty) and muxes the master
// ↔ the UI client over these typed frames (same [len:4][type:1][payload] wire):
//   FRAME_PTY_DATA   (both directions) payload = raw PTY bytes. engine→client:
//                    bytes the child emitted (feed straight into vt100). client→
//                    engine: keystrokes to write to the PTY master (input path).
//   FRAME_PTY_RESIZE (client→engine) payload = [rows:u16 BE][cols:u16 BE]; the
//                    client's pane size — engine applies it to the PTY (resize).
//   FRAME_PTY_EOF    (engine→client) empty; the child exited and the master hit
//                    EOF — the pane may freeze its last screen.
pub const FRAME_PTY_DATA: u8 = 7;
pub const FRAME_PTY_RESIZE: u8 = 8;
pub const FRAME_PTY_EOF: u8 = 9;
//
// oaita API proxy mux (`--api` boxes only). The runner serves an in-box UDS at
// /run/sarun/api.sock and frames each accepted client conn over the existing
// box-channel as a logical stream — so the engine never sees a second host UDS
// and a box-internal process has no path to ui.sock at all. Attribution is
// implicit: every frame on a box-channel is "from this box."
//   FRAME_API_OPEN  (runner→engine) payload = [u32 BE stream_id]; the runner
//                   accepted a new in-box client connection. Engine spins up
//                   an HTTP server per stream and starts feeding bytes through.
//   FRAME_API_DATA  (both directions) payload = [u32 BE stream_id][bytes];
//                   runner→engine carries HTTP request bytes; engine→runner
//                   carries HTTP response bytes (which the runner pipes back
//                   onto the box-side conn).
//   FRAME_API_CLOSE (both directions) payload = [u32 BE stream_id]; sender's
//                   half-close. The receiver finishes draining and closes
//                   its side.
pub const FRAME_API_OPEN: u8 = 10;
pub const FRAME_API_DATA: u8 = 11;
pub const FRAME_API_CLOSE: u8 = 12;
//
// FD broker — the box-channel is the rendezvous for in-box processes that
// need their OWN fresh engine connection (a nested `sarun run`, an oaita
// CLI call from a shell, etc.). The inner serves an abstract UDS inside
// the box's netns; child processes dial it and send a one-byte request;
// the inner relays the request as FRAME_OPEN_CONN over the box-channel.
// The engine then creates a fresh handler-side socketpair, spawns its
// own handler on one half, and sends the OTHER half back as
// FRAME_CONN (with SCM_RIGHTS attached). The inner forwards the fd to
// the requesting child via SCM_RIGHTS on the abstract UDS.
//
//   FRAME_OPEN_CONN  (inner→engine) empty payload — every box-channel
//                    has exactly one box id, so attribution is implicit.
//   FRAME_CONN       (engine→inner) empty payload — the actual fd is
//                    attached via SCM_RIGHTS on the sendmsg. The inner
//                    matches it positionally against the in-flight
//                    request queue.
pub const FRAME_OPEN_CONN: u8 = 13;
pub const FRAME_CONN: u8 = 14;

/// Encode `[u32 BE stream_id]` (no body) — for FRAME_API_OPEN and
/// FRAME_API_CLOSE.
#[allow(dead_code)]
pub fn api_id_payload(stream_id: u32) -> Vec<u8> {
    stream_id.to_be_bytes().to_vec()
}

/// Encode `[u32 BE stream_id][bytes]` — for FRAME_API_DATA.
#[allow(dead_code)]
pub fn api_data_payload(stream_id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + bytes.len());
    v.extend_from_slice(&stream_id.to_be_bytes());
    v.extend_from_slice(bytes);
    v
}

/// Decode the stream_id off the front of FRAME_API_DATA/OPEN/CLOSE payload.
/// Returns (stream_id, remaining_bytes). None if too short.
#[allow(dead_code)]
pub fn api_parse(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < 4 { return None; }
    let id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Some((id, &payload[4..]))
}

/// Body of a FRAME_PTY_RESIZE frame: [rows:u16 BE][cols:u16 BE].
#[allow(dead_code)]
pub fn pty_resize_payload(rows: u16, cols: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(4);
    v.extend_from_slice(&rows.to_be_bytes());
    v.extend_from_slice(&cols.to_be_bytes());
    v
}

/// Decode a FRAME_PTY_RESIZE body back to (rows, cols). None if malformed.
pub fn pty_resize_parse(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() < 4 {
        return None;
    }
    Some((
        u16::from_be_bytes([payload[0], payload[1]]),
        u16::from_be_bytes([payload[2], payload[3]]),
    ))
}

/// Encode one typed frame: [total-len:4 BE][type:1][payload].
pub fn encode(ftype: u8, payload: &[u8]) -> Vec<u8> {
    let total = (1 + payload.len()) as u32;
    let mut out = Vec::with_capacity(4 + total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.push(ftype);
    out.extend_from_slice(payload);
    out
}

/// Body of an ECHO frame: [stream:1][bytes]. stream 0=stdout, 1=stderr.
pub fn echo_payload(stream: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + data.len());
    v.push(stream);
    v.extend_from_slice(data);
    v
}

/// Decode as many whole frames as `buf` holds. Returns (frames, consumed) where
/// `consumed` is the number of leading bytes that formed whole frames; the
/// caller keeps `buf[consumed..]` as the partial-frame remainder for next time.
pub fn decode(buf: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
    let mut out = vec![];
    let mut i = 0usize;
    let n = buf.len();
    while n - i >= 4 {
        let tot = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]])
            as usize;
        if n - (i + 4) < tot {
            break; // partial frame: stop, keep remainder
        }
        if tot == 0 {
            i += 4;
            continue; // malformed-but-tolerable: zero-length frame
        }
        let ftype = buf[i + 4];
        let payload = buf[i + 5..i + 4 + tot].to_vec();
        out.push((ftype, payload));
        i += 4 + tot;
    }
    (out, i)
}
