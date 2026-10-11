//! A command-owned byte broker. Commands receive namespace TCP endpoints, never
//! connected host socket descriptors or a general-purpose destination API.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use keke_config_types::SandboxPolicy;

#[derive(Debug)]
pub(crate) struct Broker {
    directory: tempfile::TempDir,
    stopped: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    pub(crate) ports: Vec<u16>,
    pub(crate) exposed: Vec<u16>,
}

impl Broker {
    pub(crate) fn new(
        ports: Vec<u16>,
        exposed: Vec<u16>,
        policy: &SandboxPolicy,
    ) -> io::Result<Self> {
        let directory = tempfile::Builder::new().prefix("keke-network-").tempdir()?;
        let listener = UnixListener::bind(directory.path().join("broker.sock"))?;
        listener.set_nonblocking(true)?;
        let mut servers = Vec::new();
        for port in &exposed {
            let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, *port))?;
            tcp.set_nonblocking(true)?;
            let unix = UnixListener::bind(directory.path().join(format!("server-{port}.sock")))?;
            unix.set_nonblocking(true)?;
            servers.push((tcp, unix));
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let allowed = ports.clone();
        let maximum = usize::from(policy.proxy_max_connections);
        let header = Duration::from_millis(policy.proxy_header_timeout_ms);
        let connect = Duration::from_millis(policy.proxy_connect_timeout_ms);
        let idle = Duration::from_millis(policy.proxy_idle_timeout_ms);
        let poll = Duration::from_millis(policy.proxy_poll_interval_ms);
        let worker = thread::Builder::new()
            .name("linux-network-broker".into())
            .spawn(move || {
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !flag.load(Ordering::Acquire) {
                    let mut index = 0;
                    while index < workers.len() {
                        if workers[index].is_finished() {
                            let _ = workers.swap_remove(index).join();
                        } else {
                            index += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) if workers.len() < maximum => {
                            let allowed = allowed.clone();
                            let stopped = flag.clone();
                            if let Ok(worker) = thread::Builder::new()
                                .name("linux-broker-stream".into())
                                .spawn(move || {
                                    let _ = stream.set_read_timeout(Some(header));
                                    let mut request = [0; 2];
                                    if stream.read_exact(&mut request).is_err() {
                                        return;
                                    }
                                    let port = u16::from_be_bytes(request);
                                    if !allowed.contains(&port) {
                                        return;
                                    }
                                    let Ok(upstream) = TcpStream::connect_timeout(
                                        &(Ipv4Addr::LOCALHOST, port).into(),
                                        connect,
                                    ) else {
                                        return;
                                    };
                                    let _ = forward(stream, upstream, stopped, idle, poll);
                                })
                            {
                                workers.push(worker);
                            }
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(poll)
                        }
                        Err(_) => break,
                    }
                }
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        let mut workers = vec![worker];
        for (tcp, unix) in servers {
            let flag = stopped.clone();
            workers.push(thread::spawn(move || {
                while !flag.load(Ordering::Acquire) {
                    match tcp.accept() {
                        Ok((tcp, _)) => {
                            let deadline = Instant::now() + header;
                            while !flag.load(Ordering::Acquire) && Instant::now() < deadline {
                                match unix.accept() {
                                    Ok((mut unix, _)) => {
                                        if unix.write_all(&[1]).is_ok() {
                                            let _ = forward(unix, tcp, flag.clone(), idle, poll);
                                        }
                                        break;
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                        thread::sleep(poll)
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(poll)
                        }
                        Err(_) => break,
                    }
                }
            }));
        }
        Ok(Self {
            directory,
            stopped,
            workers,
            ports,
            exposed,
        })
    }

    pub(crate) fn directory(&self) -> &Path {
        self.directory.path()
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

pub(crate) fn ssh_proxy(args: Vec<std::ffi::OsString>) -> io::Error {
    if args.len() != 3 {
        return io::Error::other("SSH proxy requires proxy-port host destination-port");
    }
    let Some(proxy) = args[0]
        .to_str()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0)
    else {
        return io::Error::other("invalid SSH proxy port");
    };
    let Some(host) = args[1]
        .to_str()
        .filter(|s| !s.chars().any(char::is_whitespace))
    else {
        return io::Error::other("invalid SSH host");
    };
    let Some(port) = args[2]
        .to_str()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|p| *p != 0)
    else {
        return io::Error::other("invalid SSH destination port");
    };
    let mut stream = match TcpStream::connect((Ipv4Addr::LOCALHOST, proxy)) {
        Ok(stream) => stream,
        Err(error) => return error,
    };
    if let Err(error) = write!(
        stream,
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n"
    ) {
        return error;
    }
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") && header.len() < 16384 {
        let mut byte = [0];
        if let Err(error) = stream.read_exact(&mut byte) {
            return error;
        }
        header.push(byte[0]);
    }
    if !header.starts_with(b"HTTP/1.1 200 ") {
        return io::Error::other(format!(
            "SSH CONNECT refused: {}",
            String::from_utf8_lossy(&header)
        ));
    }
    let mut writer = match stream.try_clone() {
        Ok(stream) => stream,
        Err(error) => return error,
    };
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin(), &mut writer);
        let _ = writer.shutdown(Shutdown::Write);
    });
    match io::copy(&mut stream, &mut io::stdout()) {
        Ok(_) => std::process::exit(0),
        Err(error) => error,
    }
}

pub(crate) fn supervise(args: Vec<std::ffi::OsString>) -> io::Error {
    let mut args = args.into_iter();
    let Some(path) = args.next() else {
        return io::Error::other("missing broker socket");
    };
    let Some(maximum) = args
        .next()
        .and_then(|s| s.to_str().and_then(|s| s.parse::<usize>().ok()))
    else {
        return io::Error::other("missing relay connection limit");
    };
    let Some(idle) = args
        .next()
        .and_then(|s| s.to_str().and_then(|s| s.parse::<u64>().ok()))
        .map(Duration::from_millis)
    else {
        return io::Error::other("missing relay idle limit");
    };
    let Some(poll) = args
        .next()
        .and_then(|s| s.to_str().and_then(|s| s.parse::<u64>().ok()))
        .map(Duration::from_millis)
    else {
        return io::Error::other("missing relay poll limit");
    };
    if maximum == 0 || idle.is_zero() || poll.is_zero() {
        return io::Error::other("invalid relay budgets");
    }
    let mut ports = Vec::new();
    let mut exposed = Vec::new();
    loop {
        let Some(arg) = args.next() else {
            return io::Error::other("missing relay command");
        };
        if arg == "--" {
            break;
        }
        if arg == "--expose" {
            let Some(port) = args
                .next()
                .and_then(|s| s.to_str().and_then(|s| s.parse::<u16>().ok()))
                .filter(|p| *p != 0)
            else {
                return io::Error::other("invalid server port");
            };
            exposed.push(port);
            continue;
        }
        let Some(port) = arg
            .to_str()
            .and_then(|s| s.parse::<u16>().ok())
            .filter(|p| *p != 0)
        else {
            return io::Error::other("invalid relay port");
        };
        ports.push(port);
    }
    let Some(program) = args.next() else {
        return io::Error::other("missing relay command");
    };
    let mut listeners = Vec::new();
    for port in ports {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => listeners.push((port, listener)),
            Err(error) => return error,
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for port in exposed {
        let Some(parent) = std::path::Path::new(&path).parent() else {
            return io::Error::other("broker socket has no parent directory");
        };
        let path = parent.join(format!("server-{port}.sock"));
        let flag = stop.clone();
        threads.push(thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                let Ok(mut unix) = UnixStream::connect(&path) else {
                    thread::sleep(poll);
                    continue;
                };
                let _ = unix.set_read_timeout(Some(poll));
                let mut ack = [0];
                loop {
                    if flag.load(Ordering::Acquire) {
                        return;
                    }
                    match unix.read_exact(&mut ack) {
                        Ok(()) => break,
                        Err(error)
                            if [io::ErrorKind::WouldBlock, io::ErrorKind::TimedOut]
                                .contains(&error.kind()) => {}
                        Err(_) => return,
                    }
                }
                if let Ok(tcp) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                    let _ = forward(unix, tcp, flag.clone(), idle, poll);
                }
            }
        }));
    }
    for (port, listener) in listeners {
        let flag = stop.clone();
        let path = path.clone();
        if let Err(error) = listener.set_nonblocking(true) {
            return error;
        }
        let worker = thread::spawn(move || {
            let mut workers: Vec<JoinHandle<()>> = Vec::new();
            while !flag.load(Ordering::Acquire) {
                workers.retain(|worker| !worker.is_finished());
                match listener.accept() {
                    Ok((tcp, _)) if workers.len() < maximum => {
                        let flag = flag.clone();
                        let path = path.clone();
                        workers.push(thread::spawn(move || {
                            let Ok(mut unix) = UnixStream::connect(path) else {
                                return;
                            };
                            if unix.write_all(&port.to_be_bytes()).is_ok() {
                                let _ = forward(unix, tcp, flag, idle, poll);
                            }
                        }));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(poll),
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        threads.push(worker);
    }
    let status = std::process::Command::new(program).args(args).status();
    stop.store(true, Ordering::Release);
    for worker in threads {
        let _ = worker.join();
    }
    match status {
        Ok(status) => std::process::exit(status.code().unwrap_or(126)),
        Err(error) => error,
    }
}

// One bounded buffer per direction; nonblocking writes never make shutdown
// wait for an upstream or a command that stopped reading.
pub(crate) fn forward(
    mut unix: UnixStream,
    mut tcp: TcpStream,
    stopped: Arc<AtomicBool>,
    idle: Duration,
    poll: Duration,
) -> io::Result<()> {
    unix.set_nonblocking(true)?;
    tcp.set_nonblocking(true)?;
    let mut buffers = [[0u8; 8192]; 2];
    let mut lengths = [0usize; 2];
    let mut offsets = [0usize; 2];
    let mut ended = [false; 2];
    let mut activity = Instant::now();
    while !stopped.load(Ordering::Acquire) && activity.elapsed() < idle {
        let mut progress = false;
        for direction in 0..2 {
            if lengths[direction] == 0 && !ended[direction] {
                let read = if direction == 0 {
                    unix.read(&mut buffers[direction])
                } else {
                    tcp.read(&mut buffers[direction])
                };
                match read {
                    Ok(0) => {
                        ended[direction] = true;
                        if direction == 0 {
                            tcp.shutdown(Shutdown::Write)?;
                        } else {
                            unix.shutdown(Shutdown::Write)?;
                        }
                    }
                    Ok(count) => {
                        lengths[direction] = count;
                        offsets[direction] = 0;
                        progress = true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
            if lengths[direction] != 0 {
                let bytes = &buffers[direction][offsets[direction]..lengths[direction]];
                let write = if direction == 0 {
                    tcp.write(bytes)
                } else {
                    unix.write(bytes)
                };
                match write {
                    Ok(0) => return Ok(()),
                    Ok(count) => {
                        offsets[direction] += count;
                        progress = true;
                        if offsets[direction] == lengths[direction] {
                            lengths[direction] = 0;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if ended == [true, true] && lengths == [0, 0] {
            break;
        }
        if progress {
            activity = Instant::now();
        } else {
            thread::sleep(poll);
        }
    }
    Ok(())
}
