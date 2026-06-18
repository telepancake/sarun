// pcapng writer for the box's TAP. One Interface Description Block at start
// (link-type ETHERNET, snaplen 65535), then one Enhanced Packet Block per
// frame in either direction. We capture at the TAP, NOT at the engine's
// onward sockets — what wireshark sees IS what the box sent/received.
//
// TLS keys go to a sibling `.keys` file in NSS SSLKEYLOGFILE format so tshark
// can decrypt with `-o tls.keylog_file:flows-…keys`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use pcap_file::pcapng::PcapNgWriter;
use pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock;
use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
use pcap_file::DataLink;

pub struct FlowsLog {
    pub path: PathBuf,
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
            path, keylog_path,
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
