//! Network connection: plain TCP or TLS, behind one `Read`/`Write` type.

use crate::url::Url;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// A connection that is either a plain TCP stream or a TLS stream over TCP.
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn other<E: ToString>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Connect to the URL's host:port, upgrading to TLS for https.
///
/// `timeout` is applied to both the connect and subsequent reads. TLS
/// certificate verification is intentionally disabled to mirror the C fork's
/// default (`--ssl-verify` is off by default there); opt-in verification is a
/// planned flag.
pub fn connect(url: &Url, timeout: Duration) -> io::Result<Conn> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| other(format!("DNS: no address for {}", url.host)))?;

    let tcp = TcpStream::connect_timeout(&addr, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_nodelay(true).ok();

    if url.tls {
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(other)?;
        let tls = connector.connect(&url.host, tcp).map_err(other)?;
        Ok(Conn::Tls(Box::new(tls)))
    } else {
        Ok(Conn::Plain(tcp))
    }
}
