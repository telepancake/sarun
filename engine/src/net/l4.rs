// Generic L4 TCP forwarder: open a real upstream socket from the engine's
// (host) namespace and shuttle bytes both ways. Pcap of the box's TAP gives
// you the full view; the engine's onward leg is NOT captured here (per the
// "raw TAP frames only" design call).
//
// Errors from either side trigger a graceful shutdown of the other.

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::bridge::SmoltcpStream;

pub async fn forward(mut box_side: SmoltcpStream, host: &str, port: u16) -> Result<()> {
    let mut upstream = TcpStream::connect((host, port)).await?;
    let (mut br, mut bw) = tokio::io::split(&mut box_side);
    let (mut ur, mut uw) = upstream.split();
    let a = async {
        // A copy error here is a real transfer fault (box→upstream); surface it
        // so a truncated upload isn't invisible. The shutdown that follows is
        // best-effort teardown — a closed/reset peer there is expected.
        if let Err(e) = tokio::io::copy(&mut br, &mut uw).await {
            eprintln!("sarun-engine: net: l4 box->upstream {host}:{port}: {e}");
        }
        let _ = uw.shutdown().await;
    };
    let b = async {
        // Same in the upstream→box direction.
        if let Err(e) = tokio::io::copy(&mut ur, &mut bw).await {
            eprintln!("sarun-engine: net: l4 upstream->box {host}:{port}: {e}");
        }
        let _ = bw.shutdown().await;
    };
    tokio::join!(a, b);
    Ok(())
}
