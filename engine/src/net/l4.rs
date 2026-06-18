// Generic L4 TCP forwarder: open a real upstream socket from the engine's
// (host) namespace and shuttle bytes both ways. Pcap of the box's TAP gives
// you the full view; the engine's onward leg is NOT captured here (per the
// "raw TAP frames only" design call).
//
// Errors from either side trigger a graceful shutdown of the other.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::bridge::SmoltcpStream;

pub async fn forward(mut box_side: SmoltcpStream, host: &str, port: u16) -> Result<()> {
    let mut upstream = TcpStream::connect((host, port)).await?;
    let (mut br, mut bw) = tokio::io::split(&mut box_side);
    let (mut ur, mut uw) = upstream.split();
    let a = async {
        let _ = tokio::io::copy(&mut br, &mut uw).await;
        let _ = uw.shutdown().await;
    };
    let b = async {
        let _ = tokio::io::copy(&mut ur, &mut bw).await;
        let _ = bw.shutdown().await;
    };
    tokio::join!(a, b);
    Ok(())
}
