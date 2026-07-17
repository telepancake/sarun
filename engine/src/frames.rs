// Muxed box-channel framing over the same frugal atom abstraction as
// `tv/wire/wire.h`.
//
// After the register handshake, the box runner's single connection carries
// typed compound atoms so fd-passing and byte streams coexist on one stream:
//
//     outer_atom { type_u64_atom || payload_blob_atom }
//
// Frame types:
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
// ↔ the UI client over these typed compound atoms:
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

// QEMU registration descriptor lane.  The engine connects to the freshly
// created virtio-fs export in its own mount namespace, then passes that
// connected endpoint to the runner.  QEMU therefore never resolves an engine
// pathname and nested appliances work across the same authenticated broker
// boundary as every other nested runner.
pub const FRAME_APPLIANCE_FS: u8 = 15;

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

/// Encode one typed frame as an outer atom containing type and payload atoms.
pub fn encode(ftype: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len().saturating_add(1) <= MAX_FRAME_LEN);
    let type_storage = [ftype];
    let type_payload = if ftype == 0 { &[][..] } else { &type_storage[..] };
    let mut out = Vec::new();
    crate::wire::put_many(&mut out, &[type_payload, payload])
        .expect("a bounded in-memory frame always fits the atom format");
    out
}

/// Body of an ECHO frame: [stream:1][bytes]. stream 0=stdout, 1=stderr.
pub fn echo_payload(stream: u8, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + data.len());
    v.push(stream);
    v.extend_from_slice(data);
    v
}

/// Maximum type+payload bytes accepted for one frame. Every legitimate frame
/// is small; 16 MiB remains far above the 64 KiB stream read buffers and bounds
/// accumulation of a hostile long atom.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Decode as many whole frames as `buf` holds. Returns (frames, consumed) where
/// `consumed` is the number of leading bytes that formed whole frames; the
/// caller keeps `buf[consumed..]` as the partial-frame remainder for next time.
///
/// A malformed or oversized atom consumes the whole accumulator; the caller
/// cannot safely resynchronize and must not retain attacker-controlled bytes.
pub fn decode(buf: &[u8]) -> (Vec<(u8, Vec<u8>)>, usize) {
    let mut frames = Vec::new();
    let mut remaining = buf;
    while !remaining.is_empty() {
        let before = remaining.len();
        let outer = match crate::wire::get_atom(&mut remaining, MAX_FRAME_LEN + 16) {
            Ok(outer) => outer,
            Err(crate::wire::DecodeError::Truncated) => break,
            Err(_) => return (frames, buf.len()),
        };
        let mut fields = outer;
        let Ok(ftype) = crate::wire::get_u64(&mut fields) else {
            return (frames, buf.len());
        };
        let Ok(payload) = crate::wire::get_atom(&mut fields, MAX_FRAME_LEN) else {
            return (frames, buf.len());
        };
        if ftype > u8::MAX as u64
            || !fields.is_empty()
            || payload.len().saturating_add(1) > MAX_FRAME_LEN
        {
            return (frames, buf.len());
        }
        frames.push((ftype as u8, payload.to_vec()));
        debug_assert!(remaining.len() < before);
    }
    (frames, buf.len() - remaining.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_matches_tv_compound_atoms_and_streams() {
        let encoded = encode(FRAME_PTY_DATA, b"hello");
        assert_eq!(encoded, b"\xc7\x07\xc5hello");
        for length in 0..encoded.len() {
            assert_eq!(decode(&encoded[..length]), (Vec::new(), 0));
        }
        assert_eq!(
            decode(&encoded),
            (vec![(FRAME_PTY_DATA, b"hello".to_vec())], encoded.len()),
        );

        let first = encode(FRAME_MUTE, &[]);
        let mut joined = first.clone();
        joined.extend_from_slice(&encoded[..3]);
        assert_eq!(decode(&joined), (vec![(FRAME_MUTE, Vec::new())], first.len()));
    }

    #[test]
    fn malformed_or_oversized_frame_drops_the_accumulator() {
        // Long atom with a four-byte declared payload over the frame cap. The
        // decoder rejects it from the prefix without waiting for that payload.
        let length = MAX_FRAME_LEN + 17;
        let hostile = vec![
            0xfc,
            length as u8,
            (length >> 8) as u8,
            (length >> 16) as u8,
            (length >> 24) as u8,
        ];
        assert_eq!(decode(&hostile), (Vec::new(), hostile.len()));

        let mut malformed = Vec::new();
        crate::wire::put_many(&mut malformed, &[b"not-an-integer-width-xxxxxxxx", b""])
            .unwrap();
        assert_eq!(decode(&malformed), (Vec::new(), malformed.len()));
    }
}
