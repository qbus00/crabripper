//! Network connection: plain TCP or TLS, behind one `Read`/`Write` type.
//!
//! TLS is provided by rustls with the pure-Rust `ring` backend (no OpenSSL), so
//! the whole program cross-compiles to a fully static Linux binary. Certificates
//! are verified against the Mozilla root store by default; `--no-ssl-verify`
//! accepts any certificate (the C fork's old default) for stations with broken
//! or self-signed certs.

use crate::url::Url;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

type TlsStream = StreamOwned<ClientConnection, TcpStream>;

enum Inner {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
}

/// A connection that is either a plain TCP stream or a TLS stream over TCP.
/// While it lives, its socket is registered with the run-deadline watchdog.
pub struct Conn {
    inner: Inner,
    _watch: Watched,
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.read(buf),
            Inner::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.write(buf),
            Inner::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Plain(s) => s.flush(),
            Inner::Tls(s) => s.flush(),
        }
    }
}

// ---- run-deadline watchdog (-l) ----
//
// Read timeouts only fire when *no* byte arrives; a server trickling one byte
// at a time (or one TLS record byte by byte, which blocks inside a single
// rustls read) could keep a run alive forever. Instead, every live socket is
// registered here, and when the run's deadline passes a watchdog thread shuts
// them all down, so whatever read is blocked returns at once.

static DEADLINE: OnceLock<Instant> = OnceLock::new();

type Registry = Mutex<Vec<(u64, TcpStream)>>;

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set the absolute end of the run and start the watchdog (once).
pub fn set_deadline(deadline: Instant) {
    if DEADLINE.set(deadline).is_err() {
        return;
    }
    thread::spawn(move || {
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        loop {
            for (_, s) in registry()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
            {
                let _ = s.shutdown(Shutdown::Both);
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

fn deadline_passed() -> bool {
    DEADLINE.get().is_some_and(|d| Instant::now() >= *d)
}

/// Registry membership for one socket; removed (and the clone closed) on drop.
struct Watched(Option<u64>);

impl Watched {
    fn new(tcp: &TcpStream) -> Watched {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        if DEADLINE.get().is_none() {
            return Watched(None);
        }
        match tcp.try_clone() {
            Ok(c) => {
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                registry()
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((id, c));
                Watched(Some(id))
            }
            Err(_) => Watched(None),
        }
    }
}

impl Drop for Watched {
    fn drop(&mut self) {
        if let Some(id) = self.0 {
            registry()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|(i, _)| *i != id);
        }
    }
}

/// Resolve `host:port` with a time limit (the system resolver has none).
fn resolve(host: &str, port: u16, timeout: Duration) -> io::Result<Vec<SocketAddr>> {
    let (tx, rx) = mpsc::channel();
    let h = host.to_string();
    thread::spawn(move || {
        let _ = tx.send((h.as_str(), port).to_socket_addrs().map(|a| a.collect()));
    });
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("DNS lookup for {host} timed out"),
        )),
    }
}

fn other<E: ToString>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Connect with the outgoing socket bound to a local IP (any OS) or, on Linux,
/// an interface name.
fn connect_bound(
    addr: &std::net::SocketAddr,
    timeout: Duration,
    iface: &str,
) -> io::Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(
        Domain::for_address(*addr),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    if let Ok(ip) = iface.parse::<std::net::IpAddr>() {
        sock.bind(&std::net::SocketAddr::new(ip, 0).into())?;
    } else {
        #[cfg(target_os = "linux")]
        sock.bind_device(Some(iface.as_bytes()))?;
        #[cfg(not(target_os = "linux"))]
        return Err(other(format!(
            "-I by interface name ({iface}) is Linux-only; pass a local IP on this OS"
        )));
    }
    sock.connect_timeout(&(*addr).into(), timeout)?;
    Ok(sock.into())
}

/// A certificate verifier that accepts everything (`--no-ssl-verify`): the
/// connection is encrypted but not authenticated.
#[derive(Debug)]
struct NoVerify {
    schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes.clone()
    }
}

/// Connect to the URL's host:port, upgrading to TLS for https.
///
/// `timeout` is applied to both the connect and subsequent reads. `bind` (`-I`)
/// optionally binds the outgoing socket to a local IP, or on Linux an interface
/// name (SO_BINDTODEVICE).
pub fn connect(url: &Url, timeout: Duration, bind: Option<&str>, verify: bool) -> io::Result<Conn> {
    if deadline_passed() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "run time limit reached",
        ));
    }
    let addrs = resolve(&url.host, url.port, timeout)?;
    if addrs.is_empty() {
        return Err(other(format!("DNS: no address for {}", url.host)));
    }
    // Try each resolved address (IPv6/IPv4, several A records) in turn.
    let mut last_err = None;
    let mut tcp = None;
    for addr in &addrs {
        let r = match bind {
            None => TcpStream::connect_timeout(addr, timeout),
            Some(iface) => connect_bound(addr, timeout, iface),
        };
        match r {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let tcp = match tcp {
        Some(t) => t,
        None => return Err(last_err.unwrap_or_else(|| other("connect failed"))),
    };
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    tcp.set_nodelay(true).ok();
    let watch = Watched::new(&tcp);

    if !url.tls {
        return Ok(Conn {
            inner: Inner::Plain(tcp),
            _watch: watch,
        });
    }

    let provider = Arc::new(ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(other)?;

    // Default: real certificate + hostname verification against the Mozilla root
    // store. --no-ssl-verify: encrypted but unauthenticated.
    let config = if verify {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        let schemes = provider
            .signature_verification_algorithms
            .supported_schemes();
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify { schemes }))
            .with_no_client_auth()
    };

    let server_name = ServerName::try_from(url.host.clone())
        .map_err(|_| other(format!("invalid TLS server name: {}", url.host)))?;
    let client = ClientConnection::new(Arc::new(config), server_name).map_err(other)?;
    Ok(Conn {
        inner: Inner::Tls(Box::new(StreamOwned::new(client, tcp))),
        _watch: watch,
    })
}
