use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomEvent {
    pub event_type: EventType,
    pub agent: String,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    Claimed,
    Released,
    AgentDone,
}

// ---------------------------------------------------------------------------
// Unix implementation — Unix-domain socket IPC
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod unix_impl {
    use super::{EventType, RoomEvent};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;

    pub struct Room {
        pub(super) socket_path: PathBuf,
    }

    impl Room {
        pub fn new(grit_dir: &Path) -> Self {
            Self {
                socket_path: grit_dir.join("room.sock"),
            }
        }

        /// Send an event to the notification server.
        /// The server reads the JSON line and broadcasts it to all watchers.
        pub fn notify(&self, event: &RoomEvent) {
            if !self.socket_path.exists() {
                return;
            }
            if let Ok(mut stream) = UnixStream::connect(&self.socket_path) {
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                let json = serde_json::to_string(event).unwrap_or_default();
                let _ = writeln!(stream, "{}", json);
                let _ = stream.flush();
            }
        }
    }

    /// Notification server that listens on a Unix socket and broadcasts events.
    ///
    /// Protocol (newline-delimited JSON over Unix socket):
    /// - A connection that sends data within 200ms is a **producer** (e.g. `grit claim`).
    ///   The server reads one JSON line and broadcasts it to all watchers.
    /// - A connection that sends nothing within 200ms is a **watcher** (e.g. `grit watch`).
    ///   It stays open and receives newline-delimited JSON events.
    pub struct NotificationServer {
        pub(super) socket_path: PathBuf,
    }

    impl NotificationServer {
        pub fn new(grit_dir: &Path) -> Self {
            Self {
                socket_path: grit_dir.join("room.sock"),
            }
        }

        /// Start the notification listener in a background thread.
        /// Returns immediately. The server runs until the process exits.
        pub fn start(&self) -> anyhow::Result<()> {
            // Remove stale socket
            if self.socket_path.exists() {
                std::fs::remove_file(&self.socket_path)?;
            }

            let listener = UnixListener::bind(&self.socket_path)?;
            let watchers: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));

            let watchers_ref = watchers.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let watchers = watchers_ref.clone();
                            thread::spawn(move || {
                                handle_connection(stream, watchers);
                            });
                        }
                        Err(e) => {
                            eprintln!("Socket accept error: {}", e);
                            break;
                        }
                    }
                }
            });

            Ok(())
        }
    }

    /// Determine if a new connection is a producer or watcher, then act accordingly.
    fn handle_connection(stream: UnixStream, watchers: Arc<Mutex<Vec<UnixStream>>>) {
        // Set a short read timeout to distinguish producers from watchers
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));

        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut reader = BufReader::new(reader_stream);
        let mut line = String::new();

        match reader.read_line(&mut line) {
            Ok(n) if n > 0 => {
                // This is a producer -- broadcast the message to all watchers
                let line = line.trim().to_string();
                if !line.is_empty() {
                    broadcast_to_watchers(&watchers, &line);
                }
                // Producer connection ends here (dropped on function return)
            }
            _ => {
                // No data within timeout -- this is a watcher.
                // Clear read timeout and park the stream in the watchers list.
                // Limit max watchers to prevent resource exhaustion (DoS).
                const MAX_WATCHERS: usize = 128;
                let _ = stream.set_read_timeout(None);
                if let Ok(mut wl) = watchers.lock() {
                    if wl.len() < MAX_WATCHERS {
                        wl.push(stream);
                    }
                }
            }
        }
    }

    /// Send a message to every connected watcher, pruning dead connections.
    fn broadcast_to_watchers(watchers: &Arc<Mutex<Vec<UnixStream>>>, message: &str) {
        let mut wl = match watchers.lock() {
            Ok(wl) => wl,
            Err(_) => return,
        };

        let mut dead = Vec::new();
        for (i, watcher) in wl.iter_mut().enumerate() {
            let ok = writeln!(watcher, "{}", message).is_ok() && watcher.flush().is_ok();
            if !ok {
                dead.push(i);
            }
        }

        // Remove dead watchers in reverse index order
        for i in dead.into_iter().rev() {
            wl.remove(i);
        }
    }
}

// ---------------------------------------------------------------------------
// Windows implementation — TCP loopback IPC
//
// Windows does not support Unix-domain sockets in older SDKs and many Rust
// std builds on MSVC do not expose `std::os::unix`. We use a TCP loopback
// socket on a port stored in a `.port` file next to the DB directory.
// The protocol is identical: newline-delimited JSON; producers send one line
// then close; watchers send nothing (detected by a 200 ms read timeout) and
// stay open to receive broadcast events.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::RoomEvent;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Path of the file that records the TCP port the server is listening on.
    fn port_file(grit_dir: &Path) -> PathBuf {
        grit_dir.join("room.port")
    }

    fn read_port(grit_dir: &Path) -> Option<u16> {
        let s = std::fs::read_to_string(port_file(grit_dir)).ok()?;
        s.trim().parse().ok()
    }

    pub struct Room {
        pub(super) grit_dir: PathBuf,
    }

    impl Room {
        pub fn new(grit_dir: &Path) -> Self {
            Self {
                grit_dir: grit_dir.to_path_buf(),
            }
        }

        /// Send an event to the notification server via TCP loopback.
        pub fn notify(&self, event: &RoomEvent) {
            let Some(port) = read_port(&self.grit_dir) else {
                return;
            };
            let addr = format!("127.0.0.1:{port}");
            if let Ok(mut stream) = TcpStream::connect(&addr) {
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                let json = serde_json::to_string(event).unwrap_or_default();
                let _ = writeln!(stream, "{}", json);
                let _ = stream.flush();
            }
        }
    }

    /// Notification server that listens on a TCP loopback port and broadcasts events.
    ///
    /// Protocol (newline-delimited JSON over TCP):
    /// - A connection that sends data within 200 ms is a **producer** (e.g. `grit claim`).
    ///   The server reads one JSON line and broadcasts it to all watchers.
    /// - A connection that sends nothing within 200 ms is a **watcher** (e.g. `grit watch`).
    ///   It stays open and receives newline-delimited JSON events.
    pub struct NotificationServer {
        pub(super) grit_dir: PathBuf,
    }

    impl NotificationServer {
        pub fn new(grit_dir: &Path) -> Self {
            Self {
                grit_dir: grit_dir.to_path_buf(),
            }
        }

        /// Start the notification listener in a background thread.
        /// Binds to `127.0.0.1:0` (OS-assigned ephemeral port) and writes the
        /// chosen port number to `room.port` for clients to discover.
        pub fn start(&self) -> anyhow::Result<()> {
            // Remove a stale port file if present
            let port_path = port_file(&self.grit_dir);
            if port_path.exists() {
                std::fs::remove_file(&port_path)?;
            }

            // Bind to an OS-assigned port on loopback
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();

            // Persist the port for clients
            std::fs::write(&port_path, port.to_string())?;

            let watchers: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
            let watchers_ref = watchers.clone();

            thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let watchers = watchers_ref.clone();
                            thread::spawn(move || {
                                handle_connection(stream, watchers);
                            });
                        }
                        Err(e) => {
                            eprintln!("TCP accept error: {}", e);
                            break;
                        }
                    }
                }
            });

            Ok(())
        }
    }

    /// Determine if a new connection is a producer or watcher, then act accordingly.
    fn handle_connection(stream: TcpStream, watchers: Arc<Mutex<Vec<TcpStream>>>) {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));

        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut reader = BufReader::new(reader_stream);
        let mut line = String::new();

        match reader.read_line(&mut line) {
            Ok(n) if n > 0 => {
                let line = line.trim().to_string();
                if !line.is_empty() {
                    broadcast_to_watchers(&watchers, &line);
                }
            }
            _ => {
                const MAX_WATCHERS: usize = 128;
                let _ = stream.set_read_timeout(None);
                if let Ok(mut wl) = watchers.lock() {
                    if wl.len() < MAX_WATCHERS {
                        wl.push(stream);
                    }
                }
            }
        }
    }

    /// Send a message to every connected watcher, pruning dead connections.
    fn broadcast_to_watchers(watchers: &Arc<Mutex<Vec<TcpStream>>>, message: &str) {
        let mut wl = match watchers.lock() {
            Ok(wl) => wl,
            Err(_) => return,
        };

        let mut dead = Vec::new();
        for (i, watcher) in wl.iter_mut().enumerate() {
            let ok = writeln!(watcher, "{}", message).is_ok() && watcher.flush().is_ok();
            if !ok {
                dead.push(i);
            }
        }

        for i in dead.into_iter().rev() {
            wl.remove(i);
        }
    }
}

// ---------------------------------------------------------------------------
// Public re-exports — same surface on both platforms
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub use unix_impl::{NotificationServer, Room};

#[cfg(windows)]
pub use windows_impl::{NotificationServer, Room};

// Ensure at least one of the two cfg branches is compiled (other platforms
// would need their own impl; for now we fall back to Unix behaviour on e.g.
// macOS, which is already covered by the `unix` cfg).
