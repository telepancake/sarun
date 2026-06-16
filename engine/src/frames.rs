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
