//! HTTP download and CONNECT egress gate. DNS runs in the host, never in the sandbox.
//!
//! The OS resolver is blocking and not cancellable in std: shutdown joins it,
//! rather than detaching a thread that could outlive the policy owner.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use keke_config_types::SandboxPolicy;

const MAX_HEADER: usize = 16 * 1024;

#[derive(Debug)]
pub(crate) struct Proxy {
    port: u16,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct Policy {
    allow: Vec<String>,
    deny: Vec<String>,
    ports: Vec<u16>,
    approved: Mutex<HashSet<String>>,
    authorizer: Option<Arc<dyn crate::NetworkAuthorizer>>,
    connections: usize,
    header: Duration,
    connect: Duration,
    idle: Duration,
    poll: Duration,
}

impl Proxy {
    pub(crate) fn new(policy: &SandboxPolicy) -> io::Result<Self> {
        Self::build(policy, None, Vec::new())
    }

    pub(crate) fn new_with_authorizer(
        policy: &SandboxPolicy,
        authorizer: Arc<dyn crate::NetworkAuthorizer>,
        approved_domains: Vec<String>,
    ) -> io::Result<Self> {
        Self::build(policy, Some(authorizer), approved_domains)
    }

    fn build(
        policy: &SandboxPolicy,
        authorizer: Option<Arc<dyn crate::NetworkAuthorizer>>,
        approved_domains: Vec<String>,
    ) -> io::Result<Self> {
        let approved = approved_domains
            .iter()
            .map(|value| {
                domain(value).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid approved proxy domain")
                })
            })
            .collect::<io::Result<HashSet<_>>>()?;
        let policy = Policy {
            allow: patterns(&policy.allowed_domains)?,
            deny: patterns(&policy.denied_domains)?,
            ports: policy.proxy_allowed_ports.clone(),
            approved: Mutex::new(approved),
            authorizer,
            connections: policy.proxy_max_connections as usize,
            header: Duration::from_millis(policy.proxy_header_timeout_ms),
            connect: Duration::from_millis(policy.proxy_connect_timeout_ms),
            idle: Duration::from_millis(policy.proxy_idle_timeout_ms),
            poll: Duration::from_millis(policy.proxy_poll_interval_ms),
        };
        if policy.connections == 0
            || [policy.header, policy.connect, policy.idle, policy.poll].contains(&Duration::ZERO)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "proxy budgets must be positive",
            ));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = thread::Builder::new()
            .name("sandbox-proxy".into())
            .spawn(move || {
                let policy = Arc::new(policy);
                let mut workers: Vec<JoinHandle<()>> = Vec::new();
                while !flag.load(Ordering::Acquire) {
                    let mut i = 0;
                    while i < workers.len() {
                        if workers[i].is_finished() {
                            let _ = workers.swap_remove(i).join();
                        } else {
                            i += 1;
                        }
                    }
                    // Leave excess clients in the kernel's bounded accept backlog.
                    if workers.len() < policy.connections {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let policy = policy.clone();
                                let stop = flag.clone();
                                if let Ok(worker) = thread::Builder::new()
                                    .name("proxy-connect".into())
                                    .spawn(move || {
                                        let _ = serve(stream, &policy, &stop);
                                    })
                                {
                                    workers.push(worker);
                                }
                                continue;
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(_) => break,
                        }
                    }
                    thread::sleep(policy.poll);
                }
                flag.store(true, Ordering::Release);
                drop(listener);
                for worker in workers {
                    let _ = worker.join();
                }
            })?;
        Ok(Self {
            port,
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn domain(raw: &str) -> Option<String> {
    let name = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    if name.len() > 253
        || name.parse::<IpAddr>().is_ok()
        || name
            .split('.')
            .all(|s| s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    if !name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) {
        return None;
    }
    Some(name)
}

fn patterns(values: &[String]) -> io::Result<Vec<String>> {
    values
        .iter()
        .map(|value| {
            let (prefix, raw) = value
                .strip_prefix("*.")
                .map_or(("", value.as_str()), |s| ("*.", s));
            domain(raw)
                .map(|name| format!("{prefix}{name}"))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid proxy domain pattern")
                })
        })
        .collect()
}

fn matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        host.len() > suffix.len() && host.ends_with(suffix)
    } else {
        pattern == host
    }
}

fn allowed(policy: &Policy, host: &str) -> bool {
    if policy.deny.iter().any(|p| matches(p, host)) {
        return false;
    }
    if policy.allow.iter().any(|p| matches(p, host)) {
        return true;
    }
    // Serialize decisions so simultaneous CONNECTs cannot prompt twice for
    // the same domain. A poisoned decision fails closed.
    let Ok(mut approved) = policy.approved.lock() else {
        return false;
    };
    if approved.contains(host) {
        return true;
    }
    if policy
        .authorizer
        .as_ref()
        .is_some_and(|a| a.authorize(host))
    {
        approved.insert(host.to_owned());
        return true;
    }
    false
}

fn authority(raw: &str) -> Option<(String, u16)> {
    let (host, port) = raw.rsplit_once(':')?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let port = port.parse::<u16>().ok()?;
    if port == 0 {
        return None;
    }
    Some((domain(host)?, port))
}

#[derive(Debug, PartialEq)]
struct Request {
    host: String,
    port: u16,
    forward: Option<Vec<u8>>,
}

fn request(bytes: &[u8]) -> Result<Request, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "400 Bad Request")?;
    let text = text.strip_suffix("\r\n\r\n").ok_or("400 Bad Request")?;
    let mut lines = text.split("\r\n");
    let parts: Vec<_> = lines.next().ok_or("400 Bad Request")?.split(' ').collect();
    if parts.len() != 3 || !["HTTP/1.0", "HTTP/1.1"].contains(&parts[2]) {
        return Err("400 Bad Request");
    }
    let (host, port, mut forward) = if parts[0] == "CONNECT" {
        let (host, port) = authority(parts[1]).ok_or("400 Bad Request")?;
        (host, port, None)
    } else if ["GET", "HEAD"].contains(&parts[0]) {
        let url = parts[1].strip_prefix("http://").ok_or("400 Bad Request")?;
        let split = url.find(['/', '?']).unwrap_or(url.len());
        let raw_authority = &url[..split];
        let (host, port) = if raw_authority.contains(':') {
            authority(raw_authority).ok_or("400 Bad Request")?
        } else {
            (domain(raw_authority).ok_or("400 Bad Request")?, 80)
        };
        let target = if split == url.len() {
            "/".to_string()
        } else if url.as_bytes()[split] == b'?' {
            format!("/{}", &url[split..])
        } else {
            url[split..].to_string()
        };
        if target.contains('#') || target.bytes().any(|b| !(33..127).contains(&b)) {
            return Err("400 Bad Request");
        }
        let header = format!(
            "{} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n",
            parts[0]
        );
        (host, port, Some(header.into_bytes()))
    } else {
        return Err("405 Only HTTP GET/HEAD downloads and HTTPS CONNECT are supported");
    };
    let mut seen_host = false;
    for line in lines {
        let (key, value) = line.split_once(':').ok_or("400 Bad Request")?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value
                .bytes()
                .any(|b| b < 32 && b != b'\t' || b == 127 || b >= 128)
        {
            return Err("400 Bad Request");
        }
        if key.eq_ignore_ascii_case("host") {
            let supplied = if forward.is_none() {
                authority(value.trim())
            } else {
                if value.trim().contains(':') {
                    authority(value.trim())
                } else {
                    domain(value.trim()).map(|host| (host, 80))
                }
            };
            if seen_host || supplied != Some((host.clone(), port)) {
                return Err("400 Bad Request");
            }
            seen_host = true;
        }
        if key.eq_ignore_ascii_case("content-length")
            || key.eq_ignore_ascii_case("transfer-encoding")
            || key.eq_ignore_ascii_case("upgrade")
            || key.eq_ignore_ascii_case("expect")
            || key.eq_ignore_ascii_case("connection")
                && !["close", "keep-alive"]
                    .iter()
                    .any(|v| value.trim().eq_ignore_ascii_case(v))
        {
            return Err("400 Bad Request");
        }
        if let Some(header) = forward.as_mut()
            && ![
                "host",
                "connection",
                "proxy-connection",
                "proxy-authorization",
                "proxy-authenticate",
            ]
            .iter()
            .any(|name| key.eq_ignore_ascii_case(name))
        {
            header.extend_from_slice(line.as_bytes());
            header.extend_from_slice(b"\r\n");
        }
    }
    if let Some(header) = forward.as_mut() {
        header.extend_from_slice(b"\r\n");
    }
    Ok(Request {
        host,
        port,
        forward,
    })
}

// Conservative global-unicast allowlist; transition/translation IPv6 ranges
// are excluded so embedded IPv4 cannot bypass the IPv4 gate.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || a == 100 && (64..=127).contains(&b)
                || a == 169 && b == 254
                || a == 172 && (16..=31).contains(&b)
                || a == 192 && (b == 168 || b == 0 || b == 2)
                || a == 192 && b == 88 && c == 99
                || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
                || a == 203 && b == 0 && c == 113)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

fn retry(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

fn response(
    stream: &mut TcpStream,
    status: &str,
    policy: &Policy,
    stop: &AtomicBool,
) -> io::Result<()> {
    let bytes = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
    let mut offset = 0;
    let start = Instant::now();
    while offset < bytes.len() && !stop.load(Ordering::Acquire) && start.elapsed() < policy.header {
        match stream.write(&bytes.as_bytes()[offset..]) {
            Ok(0) => break,
            Ok(n) => offset += n,
            Err(e) if retry(&e) => thread::sleep(policy.poll),
            Err(e) => return Err(e),
        }
    }
    if offset == bytes.len() {
        Ok(())
    } else {
        Err(io::ErrorKind::TimedOut.into())
    }
}

fn serve(mut client: TcpStream, policy: &Policy, stop: &AtomicBool) -> io::Result<()> {
    client.set_nonblocking(true)?;
    let mut header = Vec::new();
    let start = Instant::now();
    // Read exactly through the delimiter, preserving any pipelined TLS bytes.
    while !header.ends_with(b"\r\n\r\n") {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if header.len() == MAX_HEADER || start.elapsed() >= policy.header {
            return response(
                &mut client,
                "400 Header too large or timed out",
                policy,
                stop,
            );
        }
        let mut byte = [0];
        match client.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => header.push(byte[0]),
            Err(e) if retry(&e) => thread::sleep(policy.poll),
            Err(e) => return Err(e),
        }
    }
    let request = match request(&header) {
        Ok(request) => request,
        Err(status) => return response(&mut client, status, policy, stop),
    };
    if !policy.ports.contains(&request.port) {
        return response(&mut client, "403 Proxy port not allowed", policy, stop);
    }
    if !allowed(policy, &request.host) {
        return response(&mut client, "403 Forbidden", policy, stop);
    }
    // Approval waiting is not header parsing, and cancellation must not start DNS.
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let addresses: Vec<SocketAddr> = match (request.host.as_str(), request.port).to_socket_addrs() {
        Ok(addresses) => addresses.collect(),
        Err(_) => return response(&mut client, "502 DNS resolution failed", policy, stop),
    };
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        return response(&mut client, "403 Forbidden", policy, stop);
    }
    let start = Instant::now();
    let mut upstream = None;
    for address in addresses {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(remaining) = policy.connect.checked_sub(start.elapsed()) else {
            break;
        };
        // Connection setup cannot be cancelled in std; the configured total
        // connection budget also bounds its contribution to shutdown latency.
        if let Ok(stream) = TcpStream::connect_timeout(&address, remaining) {
            upstream = Some(stream);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        return response(&mut client, "502 Connection failed", policy, stop);
    };
    if let Some(header) = &request.forward {
        upstream.set_write_timeout(Some(policy.header))?;
        upstream.write_all(header)?;
        // HTTP download grants never forward bodies or pipelined requests.
        // Half-closing upstream prevents a second authority being smuggled.
        upstream.shutdown(Shutdown::Write)?;
    }
    upstream.set_nonblocking(true)?;
    if request.forward.is_none() {
        response(&mut client, "200 Connection Established", policy, stop)?;
    }
    tunnel_mode(
        &mut client,
        &mut upstream,
        policy,
        stop,
        request.forward.is_some(),
    )
}

struct Pipe {
    bytes: [u8; 8192],
    begin: usize,
    end: usize,
    eof: bool,
}
impl Pipe {
    fn new() -> Self {
        Self {
            bytes: [0; 8192],
            begin: 0,
            end: 0,
            eof: false,
        }
    }
    fn step(&mut self, source: &mut TcpStream, target: &mut TcpStream) -> io::Result<bool> {
        let mut progress = false;
        if self.begin == self.end && !self.eof {
            match source.read(&mut self.bytes) {
                Ok(0) => {
                    self.eof = true;
                    target.shutdown(Shutdown::Write)?;
                }
                Ok(n) => {
                    self.begin = 0;
                    self.end = n;
                    progress = true;
                }
                Err(e) if retry(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if self.begin < self.end {
            match target.write(&self.bytes[self.begin..self.end]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.begin += n;
                    progress = true;
                }
                Err(e) if retry(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(progress)
    }
}

fn tunnel_mode(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    policy: &Policy,
    stop: &AtomicBool,
    download: bool,
) -> io::Result<()> {
    let (mut outgoing, mut incoming) = (Pipe::new(), Pipe::new());
    outgoing.eof = download;
    let mut active = Instant::now();
    while !stop.load(Ordering::Acquire) && active.elapsed() < policy.idle {
        let a = outgoing.step(client, upstream)?;
        let b = incoming.step(upstream, client)?;
        if outgoing.eof && incoming.eof {
            break;
        }
        if a || b {
            active = Instant::now();
        } else {
            thread::sleep(policy.poll);
        }
    }
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_is_not_an_apex_or_suffix_confusion() {
        assert!(matches("*.example.com", "a.b.example.com"));
        for host in ["example.com", "badexample.com", "example.com.evil"] {
            assert!(!matches("*.example.com", host));
        }
    }

    #[test]
    fn malformed_and_non_connect_requests_fail() {
        assert!(
            request(b"GET / HTTP/1.1\r\n\r\n")
                .unwrap_err()
                .contains("400")
        );
        for raw in [
            "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n",
            "CONNECT [::1]:443 HTTP/1.1\r\n\r\n",
            "CONNECT example.com:0 HTTP/1.1\r\n\r\n",
            "CONNECT example.com:65536 HTTP/1.1\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\n bad: x\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: evil.com:443\r\n\r\n",
        ] {
            assert!(request(raw.as_bytes()).is_err(), "{raw}");
        }
        assert_eq!(
            request(b"CONNECT EXAMPLE.COM.:443 HTTP/1.1\r\n\r\n").unwrap(),
            Request {
                host: "example.com".into(),
                port: 443,
                forward: None
            }
        );
    }

    #[test]
    fn http_downloads_rewrite_absolute_target_and_reject_smuggling() {
        let request = request(b"GET http://example.com:80/a?q=1 HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: secret\r\nAccept: */*\r\n\r\n").unwrap();
        assert_eq!(request.host, "example.com");
        assert_eq!(request.port, 80);
        let header = String::from_utf8(request.forward.unwrap()).unwrap();
        assert_eq!(
            header,
            "GET /a?q=1 HTTP/1.1\r\nHost: example.com:80\r\nConnection: close\r\nAccept: */*\r\n\r\n"
        );
        for raw in [
            "GET http://127.0.0.1/ HTTP/1.1\r\n\r\n",
            "GET http://user@example.com/ HTTP/1.1\r\n\r\n",
            "GET http://example.com:65536/ HTTP/1.1\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nHost: evil.com\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nContent-Length: 1\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nConnection: upgrade\r\n\r\n",
            "POST http://example.com/ HTTP/1.1\r\n\r\n",
        ] {
            assert!(super::request(raw.as_bytes()).is_err(), "{raw}");
        }
    }

    #[test]
    fn http_downloads_do_not_forward_a_second_request() {
        let client_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(client_listener.local_addr().unwrap()).unwrap();
        let (mut proxy_client, _) = client_listener.accept().unwrap();
        let upstream_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut upstream = TcpStream::connect(upstream_listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = upstream_listener.accept().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        proxy_client.set_nonblocking(true).unwrap();
        upstream.shutdown(Shutdown::Write).unwrap();
        upstream.set_nonblocking(true).unwrap();
        client
            .write_all(b"GET /evil HTTP/1.1\r\nHost: evil.example.com\r\n\r\n")
            .unwrap();
        let policy = Policy {
            allow: Vec::new(),
            deny: Vec::new(),
            ports: vec![80, 443],
            approved: Mutex::new(HashSet::new()),
            authorizer: None,
            connections: 1,
            header: Duration::from_secs(1),
            connect: Duration::from_secs(1),
            idle: Duration::from_secs(2),
            poll: Duration::from_millis(1),
        };
        let worker = thread::spawn(move || {
            tunnel_mode(
                &mut proxy_client,
                &mut upstream,
                &policy,
                &AtomicBool::new(false),
                true,
            )
            .unwrap()
        });
        let mut forwarded = Vec::new();
        server.read_to_end(&mut forwarded).unwrap();
        assert!(forwarded.is_empty(), "pipelined data reached upstream");
        server
            .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
        server.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        assert!(response.ends_with("\r\n\r\nok"));
    }

    #[test]
    fn configured_ports_are_parsed_without_changing_the_domain_identity() {
        for port in [22, 443, 8443] {
            let parsed = request(
                format!("CONNECT example.com:{port} HTTP/1.1\r\nHost: example.com:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
            assert_eq!(parsed.host, "example.com");
            assert_eq!(parsed.port, port);
            assert!(parsed.forward.is_none());
        }
        let parsed =
            request(b"GET http://example.com:8080/a HTTP/1.1\r\nHost: example.com:8080\r\n\r\n")
                .unwrap();
        assert_eq!(parsed.port, 8080);
        assert!(
            String::from_utf8(parsed.forward.unwrap())
                .unwrap()
                .contains("Host: example.com:8080")
        );
        assert!(
            request(b"CONNECT example.com:8443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n").is_err()
        );
    }

    #[test]
    fn forbidden_ports_are_refused_before_domain_authorization_or_dns() {
        struct Unexpected;
        impl crate::NetworkAuthorizer for Unexpected {
            fn authorize(&self, _: &str) -> bool {
                panic!("port denial must precede domain authorization");
            }
        }
        let policy = Policy {
            allow: Vec::new(),
            deny: Vec::new(),
            ports: vec![80, 443],
            approved: Mutex::new(HashSet::new()),
            authorizer: Some(Arc::new(Unexpected)),
            connections: 1,
            header: Duration::from_secs(1),
            connect: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            poll: Duration::from_millis(1),
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let worker =
            thread::spawn(move || serve(server, &policy, &AtomicBool::new(false)).unwrap());
        client
            .write_all(b"CONNECT unknown.invalid:22 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        worker.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 403 Proxy port not allowed"));
    }

    #[test]
    fn non_public_and_mapped_addresses_are_denied() {
        for ip in [
            "0.0.0.0",
            "10.1.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "2002:0808:0808::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn dynamic_grants_are_exact_cached_scoped_and_cannot_override_denial() {
        struct Reviewer(AtomicUsize);
        impl crate::NetworkAuthorizer for Reviewer {
            fn authorize(&self, _: &str) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
        }
        use std::sync::atomic::AtomicUsize;
        let reviewer = Arc::new(Reviewer(AtomicUsize::new(0)));
        let make_policy = || Policy {
            allow: vec!["static.example.com".into()],
            deny: vec!["blocked.example.com".into()],
            ports: vec![80, 443],
            approved: Mutex::new(HashSet::from([
                "blocked.example.com".into(),
                "preapproved.example.com".into(),
            ])),
            authorizer: Some(reviewer.clone()),
            connections: 1,
            header: Duration::from_secs(1),
            connect: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            poll: Duration::from_millis(1),
        };
        let first = make_policy();
        assert!(!allowed(&first, "blocked.example.com"));
        assert!(allowed(&first, "static.example.com"));
        assert!(allowed(&first, "preapproved.example.com"));
        assert_eq!(reviewer.0.load(Ordering::SeqCst), 0);
        assert!(allowed(&first, "new.example.com"));
        assert!(allowed(&first, "new.example.com"));
        assert_eq!(reviewer.0.load(Ordering::SeqCst), 1);
        assert!(allowed(&first, "child.new.example.com"));
        assert_eq!(reviewer.0.load(Ordering::SeqCst), 2);
        let second = make_policy();
        assert!(allowed(&second, "new.example.com"));
        assert_eq!(reviewer.0.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn denial_wins_and_unauthorized_domain_gets_403_without_dns() {
        let policy = Policy {
            allow: vec!["*.example.com".into()],
            deny: vec!["blocked.example.com".into()],
            ports: vec![80, 443],
            approved: Mutex::new(HashSet::new()),
            authorizer: None,
            connections: 1,
            header: Duration::from_secs(1),
            connect: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            poll: Duration::from_millis(1),
        };
        assert!(!allowed(&policy, "blocked.example.com"));
        assert!(allowed(&policy, "ok.example.com"));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (server, _) = listener.accept().unwrap();
        let worker =
            thread::spawn(move || serve(server, &policy, &AtomicBool::new(false)).unwrap());
        client
            .write_all(b"CONNECT does-not-exist.invalid:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));
        worker.join().unwrap();
    }
}
