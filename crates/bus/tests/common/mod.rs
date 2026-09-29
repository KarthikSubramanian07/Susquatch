//! A real `nats-server` child process for integration tests.
//!
//! The binary is looked up via `NATS_SERVER_BIN`, then `/opt/homebrew/bin/nats-server`, then
//! `PATH`. When none is found, [`NatsServer::start`] prints a skip notice and returns `None`, so
//! the calling test passes without doing anything (CI has no nats-server).

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A running (or stopped, restartable) nats-server on a fixed loopback port. Killed on drop.
pub struct NatsServer {
    bin: PathBuf,
    port: u16,
    child: Option<Child>,
}

impl NatsServer {
    /// Start a server on a free port, or `None` (with a skip notice) if no binary is available.
    pub fn start(test: &str) -> Option<Self> {
        let Some(bin) = find_binary() else {
            eprintln!("skipping {test}: nats-server not found (set NATS_SERVER_BIN)");
            return None;
        };
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("free port")
            .port();
        let mut server = Self {
            bin,
            port,
            child: None,
        };
        server.restart();
        Some(server)
    }

    /// `nats://127.0.0.1:<port>`
    pub fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.port)
    }

    /// Kill the server (SIGKILL) and wait for it to exit.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Start the server again on the same port and wait until it accepts connections.
    pub fn restart(&mut self) {
        self.stop();
        let child = Command::new(&self.bin)
            .args(["-a", "127.0.0.1", "-p", &self.port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn nats-server");
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            assert!(Instant::now() < deadline, "nats-server did not come up");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for NatsServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn find_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NATS_SERVER_BIN").map(PathBuf::from)
        && p.is_file()
    {
        return Some(p);
    }
    let brew = PathBuf::from("/opt/homebrew/bin/nats-server");
    if brew.is_file() {
        return Some(brew);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("nats-server"))
        .find(|p| p.is_file())
}
