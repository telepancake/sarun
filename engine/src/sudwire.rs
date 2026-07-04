// Rust encoder for tv's wire/TRACE format (tv/wire/wire.h,
// tv/trace/trace.h — TRACE_VERSION 3). Step-1.5 of the sud integration
// (engine/DESIGN-sud.md, WIP): the runner now IS the sud launcher, so it
// writes the stream head (version atom) and the launcher-side EV_EXIT
// events that tv's sudtrace used to emit. The decoder half lands with
// step 2 (trace ingest for per-write attribution).
//
// Atom encoding:
//   b in 0x00..=0xBF  1-byte atom, payload is the byte itself
//   b in 0xC0..=0xF7  inline atom, len = b - 0xC0 (0..=55), payload follows
//   b in 0xF8..=0xFF  long atom, lensz = b - 0xF8 LE length bytes, payload
// u64s are minimal-LE-byte blobs; i64s are zigzag-encoded u64s.
// A TRACE event is one outer atom wrapping { header atom || blob atom },
// with the seven base scalars delta-encoded per stream (EvState).

pub const TRACE_VERSION: u64 = 3;
pub const EV_EXIT: i64 = 4;
pub const EV_EXIT_EXITED: i64 = 0;
pub const EV_EXIT_SIGNALED: i64 = 1;

fn put_blob(out: &mut Vec<u8>, payload: &[u8]) {
    let n = payload.len() as u64;
    if n == 1 && payload[0] < 0xC0 {
        out.push(payload[0]);
        return;
    }
    if n <= 0x37 {
        out.push(0xC0 + n as u8);
        out.extend_from_slice(payload);
        return;
    }
    let mut lenbuf = [0u8; 8];
    let mut lensz = 0usize;
    let mut tmp = n;
    while tmp != 0 {
        lenbuf[lensz] = (tmp & 0xFF) as u8;
        lensz += 1;
        tmp >>= 8;
    }
    out.push(0xF8 + lensz as u8);
    out.extend_from_slice(&lenbuf[..lensz]);
    out.extend_from_slice(payload);
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    let mut buf = [0u8; 8];
    let mut n = 0usize;
    let mut v = v;
    while v != 0 {
        buf[n] = (v & 0xFF) as u8;
        n += 1;
        v >>= 8;
    }
    put_blob(out, &buf[..n]);
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    put_u64(out, ((v as u64) << 1) ^ ((v >> 63) as u64));
}

/// The stream head: wire_put_u64(TRACE_VERSION), written once before any
/// event atom.
pub fn version_atom() -> Vec<u8> {
    let mut v = Vec::with_capacity(2);
    put_u64(&mut v, TRACE_VERSION);
    v
}

/// Per-stream delta state (trace.h ev_state). Zero-initialised; encoder
/// and decoder step it identically.
#[derive(Default)]
pub struct EvState {
    ty: i64,
    ts_ns: i64,
    pid: i64,
    tgid: i64,
    ppid: i64,
    nspid: i64,
    nstgid: i64,
}

impl EvState {
    /// One complete event: the outer atom wrapping {header, blob}.
    /// Header = stream_id + seven delta-encoded base scalars + verbatim
    /// extras. Commits the new scalar values into self.
    #[allow(clippy::too_many_arguments)]
    pub fn build_event(&mut self, stream_id: u32, ty: i64, ts_ns: i64,
                       pid: i64, tgid: i64, ppid: i64,
                       nspid: i64, nstgid: i64,
                       extras: &[i64], blob: &[u8]) -> Vec<u8> {
        let mut hdr = Vec::with_capacity(96);
        put_u64(&mut hdr, stream_id as u64);
        put_i64(&mut hdr, ty - self.ty);
        put_i64(&mut hdr, ts_ns - self.ts_ns);
        put_i64(&mut hdr, pid - self.pid);
        put_i64(&mut hdr, tgid - self.tgid);
        put_i64(&mut hdr, ppid - self.ppid);
        put_i64(&mut hdr, nspid - self.nspid);
        put_i64(&mut hdr, nstgid - self.nstgid);
        for e in extras {
            put_i64(&mut hdr, *e);
        }
        self.ty = ty;
        self.ts_ns = ts_ns;
        self.pid = pid;
        self.tgid = tgid;
        self.ppid = ppid;
        self.nspid = nspid;
        self.nstgid = nstgid;
        // outer { hdr_atom || blob_atom }
        let mut payload = Vec::with_capacity(hdr.len() + blob.len() + 12);
        put_blob(&mut payload, &hdr);
        put_blob(&mut payload, blob);
        let mut out = Vec::with_capacity(payload.len() + 9);
        put_blob(&mut out, &payload);
        out
    }

    /// EV_EXIT with sudtrace's launcher semantics: extras =
    /// {status, code_or_signal, core_dumped, raw wstatus}, empty blob,
    /// nspid/nstgid mirroring pid/tgid (the launcher has no pidns view).
    pub fn build_exit(&mut self, stream_id: u32, ts_ns: i64,
                      pid: i64, tgid: i64, ppid: i64,
                      wstatus: i32) -> Vec<u8> {
        let extras = if libc::WIFEXITED(wstatus) {
            [EV_EXIT_EXITED, libc::WEXITSTATUS(wstatus) as i64, 0,
             wstatus as i64]
        } else if libc::WIFSIGNALED(wstatus) {
            [EV_EXIT_SIGNALED, libc::WTERMSIG(wstatus) as i64,
             libc::WCOREDUMP(wstatus) as i64, wstatus as i64]
        } else {
            [EV_EXIT_EXITED, 0, 0, wstatus as i64]
        };
        self.build_event(stream_id, EV_EXIT, ts_ns, pid, tgid, ppid,
                         pid, tgid, &extras, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal decoder mirroring wire.h, test-only.
    fn get<'a>(src: &mut &'a [u8]) -> &'a [u8] {
        let b = src[0];
        if b < 0xC0 {
            let out = &src[0..1];
            *src = &src[1..];
            return out;
        }
        if b < 0xF8 {
            let len = (b - 0xC0) as usize;
            let out = &src[1..1 + len];
            *src = &src[1 + len..];
            return out;
        }
        let lensz = (b - 0xF8) as usize;
        let mut len = 0usize;
        for i in 0..lensz {
            len |= (src[1 + i] as usize) << (8 * i);
        }
        let out = &src[1 + lensz..1 + lensz + len];
        *src = &src[1 + lensz + len..];
        out
    }
    fn get_u64(src: &mut &[u8]) -> u64 {
        let a = get(src);
        let mut v = 0u64;
        for (i, b) in a.iter().enumerate() {
            v |= (*b as u64) << (8 * i);
        }
        v
    }
    fn get_i64(src: &mut &[u8]) -> i64 {
        let u = get_u64(src);
        ((u >> 1) as i64) ^ -((u & 1) as i64)
    }

    #[test]
    fn atom_forms_match_wire_h() {
        // self-byte: single byte < 0xC0 encodes as itself
        let mut v = vec![];
        put_blob(&mut v, &[0x41]);
        assert_eq!(v, [0x41]);
        // 1-byte payload >= 0xC0 needs the inline form
        v.clear();
        put_blob(&mut v, &[0xC5]);
        assert_eq!(v, [0xC1, 0xC5]);
        // empty payload = inline len 0
        v.clear();
        put_blob(&mut v, &[]);
        assert_eq!(v, [0xC0]);
        // 56-byte payload tips into long form (lensz=1)
        v.clear();
        put_blob(&mut v, &[7u8; 56]);
        assert_eq!(v[0], 0xF9);
        assert_eq!(v[1], 56);
        assert_eq!(v.len(), 2 + 56);
        // u64 minimal-LE: 0 is the empty blob (inline len 0)
        v.clear();
        put_u64(&mut v, 0);
        assert_eq!(v, [0xC0]);
        v.clear();
        put_u64(&mut v, 0x1234);
        assert_eq!(v, [0xC2, 0x34, 0x12]);
        // zigzag: -1 -> 1, 1 -> 2
        v.clear();
        put_i64(&mut v, -1);
        assert_eq!(v, [0x01]);
        v.clear();
        put_i64(&mut v, 1);
        assert_eq!(v, [0x02]);
        // version atom: TRACE_VERSION=3 is the self-byte 0x03
        assert_eq!(version_atom(), [0x03]);
    }

    #[test]
    fn exit_event_roundtrips_with_delta_state() {
        let mut enc = EvState::default();
        let e1 = enc.build_exit(1, 1_000, 42, 42, 7, 0);        // exit 0
        let e2 = enc.build_exit(1, 2_500, 43, 43, 7, 9 << 8);   // exit 9
        // decode both against a fresh state, applying the same deltas
        let mut st = EvState::default();
        let mut expect = [(1_000i64, 42i64, 0i64, 0i64), (2_500, 43, 0, 9)];
        for (buf, (ts, pid, status, code)) in
            [e1, e2].iter().zip(expect.iter_mut()) {
            let mut s: &[u8] = buf;
            let outer = get(&mut s);
            assert!(s.is_empty());
            let mut p = outer;
            let mut hdr = get(&mut p);
            let blob = get(&mut p);
            assert!(blob.is_empty()); // EV_EXIT has no blob
            assert_eq!(get_u64(&mut hdr), 1); // stream id
            st.ty += get_i64(&mut hdr);
            st.ts_ns += get_i64(&mut hdr);
            st.pid += get_i64(&mut hdr);
            st.tgid += get_i64(&mut hdr);
            st.ppid += get_i64(&mut hdr);
            st.nspid += get_i64(&mut hdr);
            st.nstgid += get_i64(&mut hdr);
            assert_eq!(st.ty, EV_EXIT);
            assert_eq!(st.ts_ns, *ts);
            assert_eq!(st.pid, *pid);
            assert_eq!(st.tgid, *pid);
            assert_eq!(st.ppid, 7);
            // extras: status, code_or_signal, core, raw — not deltas
            assert_eq!(get_i64(&mut hdr), *status);
            assert_eq!(get_i64(&mut hdr), *code);
        }
    }
}
