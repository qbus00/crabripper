//! Network connection: plain TCP or TLS, behind one `Read`/`Write` type.
//!
//! TLS is provided by rustls with the pure-Rust `ring` backend (no OpenSSL), so
//! the whole program cross-compiles to a fully static Linux binary. Certificate
//! verification is disabled to match the C fork's default (`--ssl-verify` off);
//! opt-in verification is a planned flag.

use crate::url::Url;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};

type TlsStream = StreamOwned<ClientConnection, TcpStream>;

/// A connection that is either a plain TCP stream or a TLS stream over TCP.
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
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

/// A certificate verifier that accepts everything — the connection is encrypted
/// but not authenticated, matching the C fork's default. Verification will
/// become opt-in behind a flag.
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
/// `timeout` is applied to both the connect and subsequent reads.
pub fn connect(url: &Url, timeout: Duration) -> io::Result<Conn> {
    let addr = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| other(format!("DNS: no address for {}", url.host)))?;

    let tcp = TcpStream::connect_timeout(&addr, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_nodelay(true).ok();

    if !url.tls {
        return Ok(Conn::Plain(tcp));
    }

    let provider = Arc::new(ring::default_provider());
    let schemes = provider
        .signature_verification_algorithms
        .supported_schemes();

    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify { schemes }))
        .with_no_client_auth();

    let server_name = ServerName::try_from(url.host.clone())
        .map_err(|_| other(format!("invalid TLS server name: {}", url.host)))?;
    let client = ClientConnection::new(Arc::new(config), server_name).map_err(other)?;
    Ok(Conn::Tls(Box::new(StreamOwned::new(client, tcp))))
}
