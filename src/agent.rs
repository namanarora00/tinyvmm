//! Host side of the agent channel: two Unix sockets per VM.
//!
//!   <dir>/vm-<name>.sock   byte stream to the guest agent, through the virtio
//!                          console (the harness speaks the agent's protocol)
//!   <dir>/vm-<name>.ctl    control, one command per line:
//!                            fork N  ->  "ok <child name> ..." once the
//!                                        children exist and listen
//!                            quit    ->  this VM exits (its forks die with it)
//!
//! After a VM fork, only the forking thread exists in the child, so the
//! child opens its own sockets and threads (start() again, with its name).
//! It must also close the parent's inherited socket fds, or the harness
//! would never see EOF on the parent's connection.

use crate::virtio::VirtioConsole;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

struct SinkState {
    client: Option<UnixStream>,
    backlog: Vec<u8>, // guest output from before anyone connected
}

/// Where guest output goes: the connected client, else a backlog.
#[derive(Clone)]
pub struct ClientSink(Arc<Mutex<SinkState>>);

impl Write for ClientSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        let failed = match s.client.as_mut() {
            Some(c) => c.write_all(buf).is_err(),
            None => true,
        };
        if failed {
            s.client = None;
            s.backlog.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct Channel {
    pub sink: ClientSink,
    listener_fds: Vec<RawFd>,
}

impl Channel {
    /// In a fork child: close the parent's sockets inherited through fork().
    /// The Rust objects that own them belong to threads that don't exist
    /// here, so they will never be dropped (no double close).
    pub fn close_inherited(&self) {
        for &fd in &self.listener_fds {
            unsafe { libc::close(fd) };
        }
        if let Ok(s) = self.sink.0.try_lock() {
            if let Some(c) = &s.client {
                unsafe { libc::close(c.as_raw_fd()) };
            }
        }
    }
}

pub fn start(dir: &str, name: &str, virtio: Arc<Mutex<VirtioConsole>>, fork: fn(u64) -> Vec<String>) -> io::Result<Channel> {
    std::fs::create_dir_all(dir)?;
    let sock_path = format!("{dir}/vm-{name}.sock");
    let ctl_path = format!("{dir}/vm-{name}.ctl");
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&ctl_path);
    let console = UnixListener::bind(&sock_path)?;
    let control = UnixListener::bind(&ctl_path)?;
    let sink = ClientSink(Arc::new(Mutex::new(SinkState { client: None, backlog: Vec::new() })));
    let listener_fds = vec![console.as_raw_fd(), control.as_raw_fd()];

    // Console: one client at a time; a new connection replaces the old one.
    let s = sink.clone();
    std::thread::spawn(move || {
        for conn in console.incoming() {
            let Ok(mut conn) = conn else { continue };
            {
                let mut st = s.0.lock().unwrap();
                let backlog = std::mem::take(&mut st.backlog);
                let _ = conn.write_all(&backlog);
                st.client = conn.try_clone().ok();
            }
            let v = virtio.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => v.lock().unwrap().push_input(&buf[..n]),
                    }
                }
            });
        }
    });

    // Control.
    std::thread::spawn(move || {
        for conn in control.incoming() {
            let Ok(conn) = conn else { continue };
            let mut w = conn.try_clone().unwrap();
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some("fork"), Some(n)) => {
                        let names = fork(n.parse().unwrap_or(1));
                        let _ = writeln!(w, "ok {}", names.join(" "));
                    }
                    (Some("quit"), _) => {
                        let _ = writeln!(w, "bye");
                        std::process::exit(0);
                    }
                    _ => {
                        let _ = writeln!(w, "error: unknown command {line:?}");
                    }
                }
            }
        }
    });

    Ok(Channel { sink, listener_fds })
}
