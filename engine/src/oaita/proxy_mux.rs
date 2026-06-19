// Per-box-channel oaita API mux. Plugs into the existing FRAME_API_*
// handling on the runner↔engine box channel: the runner accepts in-box
// connections to /run/sarun/api.sock and frames each one as a logical
// stream (FRAME_API_OPEN(id), then FRAME_API_DATA(id, ...) in both
// directions, FRAME_API_CLOSE(id) on either side's hangup). This module
// owns the engine-side demuxer — it lives per box-channel, so attribution
// is implicit (every stream on this mux belongs to THIS box id) and the
// LLM-API path never opens a second host UDS or another control conn.
//
// Architecture per stream:
//
//     runner ───FRAME_API_DATA(id, req)──►  ApiMux  ───duplex.write──►  hyper
//                                            │                              │
//                                            ▼                              ▼
//     runner ◄──FRAME_API_DATA(id, resp)──  drain task                 serve_one_conn
//                                                                          │
//     runner ◄──FRAME_API_CLOSE(id)──────  drain task EOF              proxy::handle_inner
//                                                                          │
//                                                                          ▼
//                                                                    upstream LLM
//
// `tokio::io::duplex` gives us a connected pair the engine writes the
// runner's bytes into and hyper reads HTTP from; hyper's response writes
// go back through the same duplex pair, the drain task picks them up and
// re-frames them onto the box-channel.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::oaita::proxy::Proxy;

/// Capacity of the request mpsc (bytes batches, not bytes) and of the
/// duplex pipe. 64KB on either side is plenty for normal LLM chat traffic
/// and provides enough backpressure that an unresponsive upstream can't
/// drown the runner.
const STREAM_BUFFER: usize = 64 * 1024;
const MPSC_CAP: usize = 64;

pub struct ApiMux {
    pub box_id: i64,
    pub proxy: Arc<Proxy>,
    pub rt: tokio::runtime::Handle,
    /// Writer back to the runner — same UDS the rest of the box-channel
    /// frames go on. Each FRAME_API_DATA / FRAME_API_CLOSE we send to the
    /// runner uses this lock; framing is short so contention is minimal.
    pub channel_writer: Arc<Mutex<UnixStream>>,
    /// Per-stream request-side sender. Arc/Mutex so the response drain
    /// task can remove its own entry on EOF without re-borrowing `&self`.
    streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Bytes>>>>,
}

impl ApiMux {
    pub fn new(box_id: i64, proxy: Arc<Proxy>, rt: tokio::runtime::Handle,
               channel_writer: Arc<Mutex<UnixStream>>) -> Self {
        Self {
            box_id, proxy, rt, channel_writer,
            streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// FRAME_API_OPEN arrived from runner — spin up the per-stream HTTP
    /// server task and remember how to feed it more bytes.
    pub fn open(&self, stream_id: u32) {
        let (req_tx, mut req_rx) = mpsc::channel::<Bytes>(MPSC_CAP);
        self.streams.lock().unwrap().insert(stream_id, req_tx);
        let (client, server) = tokio::io::duplex(STREAM_BUFFER);
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let box_id = self.box_id;
        let proxy = self.proxy.clone();
        let writer = self.channel_writer.clone();
        let streams_for_close = self.streams.clone();

        // (1) Pump runner→hyper: drain the mpsc into the duplex's runner side.
        //     mpsc EOF (sender dropped via FRAME_API_CLOSE) closes the write
        //     half — hyper then sees its read half EOF and returns.
        self.rt.spawn(async move {
            while let Some(bytes) = req_rx.recv().await {
                if client_w.write_all(&bytes).await.is_err() { break; }
            }
            let _ = client_w.shutdown().await;
        });

        // (2) Pump hyper→runner: read response bytes out of the duplex and
        //     frame each chunk as FRAME_API_DATA(id, bytes). On EOF or
        //     error, send FRAME_API_CLOSE(id) and forget the stream so its
        //     mpsc sender drops if the runner hasn't already closed.
        let writer_for_drain = writer.clone();
        self.rt.spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match client_r.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let payload = crate::frames::api_data_payload(
                            stream_id, &buf[..n]);
                        let frame = crate::frames::encode(
                            crate::frames::FRAME_API_DATA, &payload);
                        let mut w = writer_for_drain.lock().unwrap();
                        if w.write_all(&frame).is_err() { break; }
                    }
                }
            }
            let payload = crate::frames::api_id_payload(stream_id);
            let frame = crate::frames::encode(
                crate::frames::FRAME_API_CLOSE, &payload);
            if let Ok(mut w) = writer.lock() { let _ = w.write_all(&frame); }
            streams_for_close.lock().unwrap().remove(&stream_id);
        });

        // (3) Serve HTTP on the engine side of the duplex. proxy logic
        //     handles auth, body parsing, upstream call, streaming
        //     response, and api_log.
        let io = hyper_util::rt::TokioIo::new(server);
        self.rt.spawn(async move {
            let _ = crate::oaita::proxy::serve_one_conn_for_box(
                proxy, box_id, io).await;
        });
    }

    /// FRAME_API_DATA(stream_id, bytes) arrived from runner — push bytes
    /// into that stream's request mpsc. If the stream is unknown (open
    /// raced ahead of data, or close already happened), drop quietly.
    pub fn data(&self, stream_id: u32, bytes: &[u8]) {
        let sender = self.streams.lock().unwrap().get(&stream_id).cloned();
        let Some(sender) = sender else { return; };
        let bytes = Bytes::copy_from_slice(bytes);
        // Try_send: we picked a small bounded queue for backpressure but
        // blocking the sync frame loop on a full mpsc would deadlock with
        // the response drain task. Drop on overflow — the upstream HTTP
        // semantics are clean if either side stalls.
        let _ = sender.try_send(bytes);
    }

    /// FRAME_API_CLOSE(stream_id) arrived from runner — drop the sender;
    /// the request pump task sees EOF, hyper's request body completes,
    /// the rest of the pipeline drains naturally.
    pub fn close(&self, stream_id: u32) {
        self.streams.lock().unwrap().remove(&stream_id);
    }

    /// Drop every active stream when the box channel hits EOF — close
    /// half-implicit via the senders' Drop on map clear.
    pub fn shutdown(&self) {
        self.streams.lock().unwrap().clear();
    }
}
