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
// Frame types 10/11/12 are retired (formerly FRAME_API_OPEN/DATA/CLOSE for
// the in-box `/run/sarun/api.sock` LLM-API mux; the FD broker subsumes
// them — each LLM call now rides a dedicated engine conn dialed via the
// broker, no per-stream demux needed).
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

/// Maximum total-len (type byte + payload) we will accept for one frame. The
/// wire prefix is a raw u32, so without a cap a box could declare a length up
/// toward 4 GiB and the reader's accumulator would keep growing as it waits for
/// the (never-arriving) rest of the frame — a memory-exhaustion lever from
/// inside the box (audit L3). Every legitimate frame is small: control frames
/// are a handful of bytes, and the largest payloads (ECHO / PTY_DATA) are
/// bounded by the readers' 64 KiB recv buffers. 16 MiB is far above any real
/// frame yet far below a buffer-growth DoS, so an over-cap length can only be a
/// bug or an attack.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Decode as many whole frames as `buf` holds. Returns (frames, consumed) where
/// `consumed` is the number of leading bytes that formed whole frames; the
/// caller keeps `buf[consumed..]` as the partial-frame remainder for next time.
///
/// A declared total-len exceeding [`MAX_FRAME_LEN`] is rejected rather than
/// trusted: we never allocate (or wait to accumulate) an oversized payload.
/// Such a length means the stream is corrupt or hostile, so we stop and report
/// the WHOLE buffer as consumed — the caller drains its accumulator (bounding
/// its memory) and the now-desynced connection drops on its next read. This is
/// the only safe move on a length-prefixed stream once a frame boundary is no
/// longer trustworthy; silently skipping the header would just desync further.
pub fn decode(buf: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
    let mut out = vec![];
    let mut i = 0usize;
    let n = buf.len();
    while n - i >= 4 {
        let tot = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]])
            as usize;
        if tot > MAX_FRAME_LEN {
            // Oversized declared length: refuse it. Consume everything so the
            // caller's accumulator cannot grow toward 4 GiB waiting for bytes
            // that must not be allocated.
            return (out, n);
        }
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
