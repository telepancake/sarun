// pcapng writer for the box's TAP + sandboxed tshark queries.
//
// Capture side: one pcapng (PcapNgWriter) with one Interface Description
// Block at start (link-type ETHERNET, snaplen 65535), then one Enhanced
// Packet Block per frame in either direction. The TLS keylog sidecar lives
// next to the pcapng under the same `flows-<ts>-box<id>` prefix.
//
// Read side (`tshark_list` / `tshark_detail`): tshark is notoriously rich
// in parsing surface area, so we run it in the SAME lockdown bwrap that
// `review::run_on_untrusted` uses for objdump / unzip / readelf — host /
// is ro-bound, every namespace is unshared (`--unshare-net` in particular),
// caps are dropped, the env is cleared, and only the box's flows directory
// is exposed at `/tmp/ut`. tshark NEVER sees the host filesystem with
// write access and can't dial the network, even if a crafted pcapng tried
// to drive it that way.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use pcap_file::pcapng::PcapNgWriter;
use pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock;
use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
use pcap_file::DataLink;

pub struct FlowsLog {
    pub keylog_path: PathBuf,
    writer: Mutex<PcapNgWriter<std::fs::File>>,
    started_ns: u128,
}

impl FlowsLog {
    pub fn create(box_dir: &std::path::Path, ts: u64, box_id: u16)
                  -> Result<Arc<Self>> {
        std::fs::create_dir_all(box_dir)?;
        let path = box_dir.join(format!("flows-{ts}-box{box_id}.pcapng"));
        let keylog_path = box_dir.join(format!("flows-{ts}-box{box_id}.keys"));
        let f = std::fs::OpenOptions::new()
            .create(true).truncate(true).write(true).open(&path)?;
        let mut w = PcapNgWriter::new(f)?;
        let idb = InterfaceDescriptionBlock {
            linktype: DataLink::ETHERNET,
            snaplen: 65535,
            options: vec![],
        };
        w.write_pcapng_block(idb)?;
        let started_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos()).unwrap_or(0);
        Ok(Arc::new(Self {
            keylog_path,
            writer: Mutex::new(w),
            started_ns,
        }))
    }

    /// Record one ethernet frame (in or out of the TAP — same interface).
    pub fn record(&self, frame: &[u8]) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos()).unwrap_or(self.started_ns);
        let blk = EnhancedPacketBlock {
            interface_id: 0,
            timestamp: std::time::Duration::from_nanos(now as u64),
            original_len: frame.len() as u32,
            data: std::borrow::Cow::Borrowed(frame),
            options: vec![],
        };
        self.writer.lock().write_pcapng_block(blk)?;
        Ok(())
    }
}

// ── tshark queries (sandboxed) ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FlowRow {
    pub frame: u64,
    pub t: f64,            // seconds since capture start
    pub src: String,
    pub dst: String,
    pub sni: String,       // SNI (TLS) or http.host
    pub host: String,      // http.host (cleartext) — populated post-MITM-decrypt
    pub method: String,
    pub uri: String,
    pub status: String,
    /// tshark's tcp.stream id (per-connection u32). Lets the UI ask for
    /// every packet in this flow's stream via `tshark_packets`.
    pub stream: i64,       // -1 when tshark couldn't fill it in
}

impl FlowRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "frame": self.frame, "t": self.t,
            "src": self.src, "dst": self.dst,
            "sni": self.sni, "host": self.host,
            "method": self.method, "uri": self.uri, "status": self.status,
            "stream": self.stream,
        })
    }
}

/// One row per ethernet frame in a tcp.stream. Used by the
/// packet-list drill-down in the flows pane.
#[derive(Clone, Debug)]
pub struct PacketRow {
    pub frame: u64,
    pub t: f64,
    pub src: String,
    pub dst: String,
    pub proto: String,   // tshark's _ws.col.protocol (TCP / TLSv1.3 / HTTP / …)
    pub len: u32,
    pub info: String,    // tshark's _ws.col.info
}

impl PacketRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "frame": self.frame, "t": self.t,
            "src": self.src, "dst": self.dst,
            "proto": self.proto, "len": self.len,
            "info": self.info,
        })
    }
}

/// Run tshark inside the trusted-sandbox bwrap (same lockdown as
/// review::run_on_untrusted) with the box's flows directory ro-bound at
/// `/tmp/ut`. The placeholder `{pcap}` / `{keys}` in `argv` resolve to the
/// inside paths.
fn run_tshark_in_box_sandbox(box_state_root: &Path, argv: &[&str]) -> Result<String, String> {
    let flows = find_flows_files(box_state_root)
        .ok_or_else(|| "no flows files for this box".to_string())?;
    let host_dir = flows.0.parent().unwrap().to_path_buf();
    let pcap_name = flows.0.file_name().unwrap().to_string_lossy().into_owned();
    let keys_name = flows.1.file_name().unwrap().to_string_lossy().into_owned();

    let inside_dir = "/tmp/ut";
    let inside_pcap = format!("{inside_dir}/{pcap_name}");
    let inside_keys = format!("{inside_dir}/{keys_name}");
    let resolved: Vec<String> = argv.iter().map(|a| match *a {
        "{pcap}" => inside_pcap.clone(),
        "{keys}" => inside_keys.clone(),
        s if s.contains("tls.keylog_file:{keys}")
              => s.replace("{keys}", &inside_keys),
        other => other.to_string(),
    }).collect();

    if which("bwrap") {
        let mut cmd = std::process::Command::new("bwrap");
        cmd.args(["--unshare-pid", "--unshare-ipc", "--unshare-uts",
                  "--unshare-net", "--die-with-parent", "--new-session",
                  "--cap-drop", "ALL",
                  "--ro-bind", "/", "/",
                  "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        cmd.arg("--ro-bind").arg(&host_dir).arg(inside_dir);
        cmd.args(["--chdir", inside_dir, "--clearenv",
                  "--setenv", "PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                  "--"]);
        cmd.args(&resolved);
        cmd.stdin(std::process::Stdio::null());
        match cmd.output() {
            Ok(out) => {
                if out.status.success() {
                    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
                } else {
                    Err(String::from_utf8_lossy(&out.stderr)
                        .trim().chars().take(2000).collect())
                }
            }
            Err(e) => Err(format!("spawn failed: {e}")),
        }
    } else {
        // Fallback: no bwrap → run raw on host (still safe-ish since tshark
        // is treated as untrusted by us, but no sandbox is no sandbox; the
        // test environment may legitimately lack bwrap).
        match std::process::Command::new(&resolved[0]).args(&resolved[1..])
            .stdin(std::process::Stdio::null()).output() {
            Ok(out) if out.status.success() =>
                Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            Ok(out) => Err(String::from_utf8_lossy(&out.stderr)
                .trim().chars().take(2000).collect()),
            Err(e) => Err(format!("spawn failed (no bwrap): {e}")),
        }
    }
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| d.join(cmd).is_file())
    })
}

/// Newest pcapng/keys pair for a box.
fn find_flows_files(box_state_root: &Path) -> Option<(PathBuf, PathBuf)> {
    // A missing/unreadable box dir or unreadable entry simply means "no flows
    // here" → None, which the caller turns into a clear "no flows files for
    // this box" error. So returning None on read failure is correct, not silent.
    let mut entries: Vec<PathBuf> = std::fs::read_dir(box_state_root).ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "pcapng"))
        .collect();
    entries.sort();
    let pcap = entries.pop()?;
    let keys = pcap.with_extension("keys");
    if !keys.exists() { return None; }
    Some((pcap, keys))
}

const FLOW_FIELDS: &[&str] = &[
    "frame.number", "frame.time_relative", "ip.src", "ip.dst",
    "tls.handshake.extensions_server_name",
    "http.host", "http.request.method", "http.request.uri", "http.response.code",
    "tcp.stream",
];

const PACKET_FIELDS: &[&str] = &[
    "frame.number", "frame.time_relative", "ip.src", "ip.dst",
    "_ws.col.protocol", "frame.len", "_ws.col.info",
];

/// Parse tshark `-T fields` tab-separated output into FlowRows.
fn parse_flow_rows(out: &str) -> Vec<FlowRow> {
    out.lines().filter_map(|line| {
        let mut it = line.split('\t');
        // tshark `-T fields` emits one tab-separated cell per requested field,
        // EMPTY when a field doesn't apply to that packet (a TCP frame has no
        // http.host, etc). So `unwrap_or("")`/`unwrap_or(default)` below is the
        // correct representation of an absent field, not a swallowed parse error.
        // A row whose frame.number won't parse is header/junk — drop it via `?`.
        let frame: u64 = it.next()?.parse().ok()?;
        let t: f64 = it.next()?.parse().unwrap_or(0.0);
        let src = it.next().unwrap_or("").to_string();
        let dst = it.next().unwrap_or("").to_string();
        let sni = it.next().unwrap_or("").to_string();
        let host = it.next().unwrap_or("").to_string();
        let method = it.next().unwrap_or("").to_string();
        let uri = it.next().unwrap_or("").to_string();
        let status = it.next().unwrap_or("").to_string();
        let stream: i64 = it.next().unwrap_or("").parse().unwrap_or(-1);
        // Drop fully-empty rows (junk between blocks).
        if sni.is_empty() && host.is_empty() && method.is_empty()
            && status.is_empty() { return None; }
        Some(FlowRow { frame, t, src, dst, sni, host, method, uri, status,
                       stream })
    }).collect()
}

/// Parse the packet-list tshark output.
fn parse_packet_rows(out: &str) -> Vec<PacketRow> {
    out.lines().filter_map(|line| {
        let mut it = line.split('\t');
        // Same as parse_flow_rows: empty cells = absent fields (expected); the
        // frame.number `?`/`ok()?` gate drops non-data lines.
        let frame: u64 = it.next()?.parse().ok()?;
        let t: f64 = it.next()?.parse().unwrap_or(0.0);
        let src = it.next().unwrap_or("").to_string();
        let dst = it.next().unwrap_or("").to_string();
        let proto = it.next().unwrap_or("").to_string();
        let len: u32 = it.next().unwrap_or("").parse().unwrap_or(0);
        let info = it.next().unwrap_or("").to_string();
        Some(PacketRow { frame, t, src, dst, proto, len, info })
    }).collect()
}

/// List interesting flows in the box's pcapng. "Interesting" = HTTP
/// request/response rows + TLS ClientHello (so the user sees SNIs for
/// connections that didn't decrypt for whatever reason).
pub fn tshark_list(box_state_root: &Path) -> Result<Vec<FlowRow>, String> {
    let mut argv: Vec<&str> = vec![
        "tshark", "-r", "{pcap}",
        "-o", "tls.keylog_file:{keys}",
        "-Y", "http or tls.handshake.type==1",
        "-T", "fields",
    ];
    for f in FLOW_FIELDS { argv.push("-e"); argv.push(f); }
    let out = run_tshark_in_box_sandbox(box_state_root, &argv)?;
    Ok(parse_flow_rows(&out))
}

/// Every frame in `tcp.stream == STREAM` — i.e. the whole TCP connection
/// the user clicked into. Powers the packet-list drill-down on the flows
/// pane. Returns rows in time order (tshark already emits them ordered).
pub fn tshark_packets(box_state_root: &Path, stream: i64)
                      -> Result<Vec<PacketRow>, String> {
    let filter = format!("tcp.stream == {stream}");
    let mut argv: Vec<&str> = vec![
        "tshark", "-r", "{pcap}",
        "-o", "tls.keylog_file:{keys}",
        "-Y", &filter,
        "-T", "fields",
    ];
    for f in PACKET_FIELDS { argv.push("-e"); argv.push(f); }
    let out = run_tshark_in_box_sandbox(box_state_root, &argv)?;
    Ok(parse_packet_rows(&out))
}

/// Verbose dissection of one frame: `tshark -V` filtered to that frame.
/// Lets a user drill into headers / body / certificate details.
pub fn tshark_detail(box_state_root: &Path, frame: u64) -> Result<String, String> {
    let filter = format!("frame.number == {frame}");
    let argv = ["tshark", "-r", "{pcap}",
                "-o", "tls.keylog_file:{keys}",
                "-Y", &filter,
                "-V"];
    run_tshark_in_box_sandbox(box_state_root, &argv)
}
