// SmoltcpStream — an AsyncRead+AsyncWrite adapter over a smoltcp TCP socket
// handle. Owned by exactly one tokio task. Reads pull from a per-conn mpsc
// receiver fed by the stack's poll thread; writes push commands into the
// stack's global cmd channel.
//
// This is intentionally simple: the poll thread already chunks bytes into
// Vec<u8> on receive, so we just buffer them here for AsyncRead. Writes are
// fire-and-forget (the poll thread queues them into smoltcp's TX buffer on
// next poll). Close is a Cmd::Close that the poll thread surfaces to the
// peer via a smoltcp::tcp::Socket::close().

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use smoltcp::iface::SocketHandle;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::stack::StackRuntime;

pub struct SmoltcpStream {
    stack: Arc<StackRuntime>,
    handle: SocketHandle,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    leftover: parking_lot::Mutex<Vec<u8>>,
    closed: std::sync::atomic::AtomicBool,
}

impl SmoltcpStream {
    pub fn new(stack: Arc<StackRuntime>, handle: SocketHandle) -> Self {
        // Bridge the stack's std::mpsc to a tokio mpsc by spawning a relay
        // thread. (We can't await on a std::mpsc::Receiver, and changing
        // the stack to publish over tokio channels would entangle threading.)
        let (tx_std, rx_std) = std::sync::mpsc::channel::<Vec<u8>>();
        let (tx_tk, rx_tk) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        stack.register_rx(handle, tx_std);
        std::thread::spawn(move || {
            while let Ok(chunk) = rx_std.recv() {
                if tx_tk.send(chunk).is_err() {
                    break;
                }
            }
        });
        Self {
            stack,
            handle,
            rx: tokio::sync::Mutex::new(rx_tk),
            leftover: parking_lot::Mutex::new(Vec::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl AsyncRead for SmoltcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        {
            let mut left = this.leftover.lock();
            if !left.is_empty() {
                let n = buf.remaining().min(left.len());
                buf.put_slice(&left[..n]);
                left.drain(..n);
                return Poll::Ready(Ok(()));
            }
        }
        // Poll the tokio receiver.
        let mut rx = match this.rx.try_lock() {
            Ok(g) => g,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };
        match rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) => {
                let n = buf.remaining().min(chunk.len());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    this.leftover.lock().extend_from_slice(&chunk[n..]);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())), // EOF
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for SmoltcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        self.stack.write(self.handle, buf.to_vec());
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.stack.close(self.handle);
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for SmoltcpStream {
    fn drop(&mut self) {
        if !self.closed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            self.stack.close(self.handle);
        }
    }
}
