//! Minimal URL parsing for shoutcast/icecast stream URLs.
//!
//! Deliberately dependency-free: we only need scheme/host/port/path, not a full
//! RFC 3986 parser. Userinfo (`user:pass@`) is stripped and ignored for now.

#[derive(Debug, Clone)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub tls: bool,
}

pub fn parse(input: &str) -> Result<Url, String> {
    let input = input.trim();
    // URLs also arrive from servers (redirect Location, HLS playlists) and are
    // interpolated into the request line, so refuse anything that could break
    // out of it (CR/LF and other controls). Literal spaces are percent-encoded.
    if input.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return Err("URL contains control characters".to_string());
    }
    let encoded = input.replace(' ', "%20");
    let input = encoded.as_str();

    let (scheme, rest) = match input.find("://") {
        Some(i) => (input[..i].to_lowercase(), &input[i + 3..]),
        None => ("http".to_string(), input),
    };
    let tls = match scheme.as_str() {
        "http" => false,
        "https" => true,
        other => return Err(format!("unsupported URL scheme: {other}")),
    };

    // Split host[:port] from the path.
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };

    // Drop any userinfo before '@'.
    let hostport = match hostport.rfind('@') {
        Some(i) => &hostport[i + 1..],
        None => hostport,
    };

    // host:port (no IPv6-literal support yet).
    let (host, port) = match hostport.rfind(':') {
        Some(i) => {
            let p: u16 = hostport[i + 1..]
                .parse()
                .map_err(|_| format!("invalid port in URL: {}", &hostport[i + 1..]))?;
            (hostport[..i].to_string(), p)
        }
        None => (hostport.to_string(), if tls { 443 } else { 80 }),
    };

    if host.is_empty() {
        return Err("URL has no host".to_string());
    }

    Ok(Url {
        scheme,
        host,
        port,
        path,
        tls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http() {
        let u = parse("http://stream4.nadaje.com:9680/radiokrakow-s3").unwrap();
        assert!(!u.tls);
        assert_eq!(u.host, "stream4.nadaje.com");
        assert_eq!(u.port, 9680);
        assert_eq!(u.path, "/radiokrakow-s3");
    }

    #[test]
    fn https_default_port_and_path() {
        let u = parse("https://example.com").unwrap();
        assert!(u.tls);
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn rejects_request_injection() {
        assert!(parse("http://h/x\r\nHost: evil").is_err());
        assert_eq!(parse("http://h/a b").unwrap().path, "/a%20b");
    }

    #[test]
    fn scheme_optional() {
        let u = parse("example.com:8000/live").unwrap();
        assert_eq!(u.port, 8000);
        assert_eq!(u.path, "/live");
    }
}
