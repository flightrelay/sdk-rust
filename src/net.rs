//! A TCP connect that can be canceled, shared by the driver helpers and the
//! endpoint.

use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

/// How often the wait looks at its `keep_going` condition.
const POLL: Duration = Duration::from_millis(50);

/// Connect to `addr` within `timeout` on a thread of its own, polling
/// `keep_going` every 50 ms. Returns `Ok(None)` as soon as it says `false`; a
/// connect that completes after that is dropped, which closes it.
pub(crate) fn connect_cancelable(
    addr: SocketAddr,
    timeout: Duration,
    mut keep_going: impl FnMut() -> bool,
) -> std::io::Result<Option<TcpStream>> {
    let (tx, rx) = sync_channel(1);
    std::thread::Builder::new()
        .name("frp-connect".into())
        .spawn(move || {
            // Nobody waits for the result once the caller gave up: the
            // stream is dropped, closing it.
            let _ = tx.send(TcpStream::connect_timeout(&addr, timeout));
        })?;
    loop {
        if !keep_going() {
            return Ok(None);
        }
        match rx.recv_timeout(POLL) {
            Ok(result) => return result.map(Some),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("connect thread ended"));
            }
        }
    }
}
