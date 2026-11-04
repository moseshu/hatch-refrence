//! The stdio transport for [`Server`](crate::server::Server): a second
//! `impl` block, kept here only to keep `server.rs` under the 400-line
//! limit (same split as `http_routes.rs` is for `http_server.rs`).

use crate::server::Server;
use hatch_core::Result;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

impl Server {
    /// Serve JSON-RPC 2.0 over stdin/stdout until stdin EOF or `shutdown`.
    /// Replies are written from this thread only; stdout is the protocol.
    pub fn run_stdio(&self, shutdown: &AtomicBool) -> Result<()> {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: the client went away.
                    Ok(_) => {
                        if tx.send(line.trim_end().to_string()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Some(reply) = self.handle_line(&line) {
                        writeln!(out, "{reply}")?;
                        out.flush()?;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
}
