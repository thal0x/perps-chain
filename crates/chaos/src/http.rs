//! Minimal blocking HTTP/1.1 client (and a tiny server used by the fake
//! node). The harness only talks to localhost admin endpoints, so a
//! dependency-free `Connection: close` client keeps it independent of any
//! async runtime and lets each poller thread use short hard timeouts.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn resolve(addr: &str) -> std::io::Result<SocketAddr> {
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, format!("cannot resolve {addr}")))
}

/// Perform one request. `addr` is `host:port`.
pub fn request(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<Response> {
    let sa = resolve(addr)?;
    let mut s = TcpStream::connect_timeout(&sa, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let _ = s.set_nodelay(true);
    let body = body.unwrap_or(&[]);
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: */*\r\n"
    );
    if method != "GET" || !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut buf = req.into_bytes();
    buf.extend_from_slice(body);
    s.write_all(&buf)?;
    s.flush()?;
    let mut r = BufReader::new(s);
    read_response(&mut r)
}

pub fn get(addr: &str, path: &str, timeout: Duration) -> std::io::Result<Response> {
    request(addr, "GET", path, None, timeout)
}

pub fn post_json(addr: &str, path: &str, body: &str, timeout: Duration) -> std::io::Result<Response> {
    request(addr, "POST", path, Some(body.as_bytes()), timeout)
}

fn bad(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

fn read_headers<R: BufRead>(r: &mut R) -> std::io::Result<Vec<(String, String)>> {
    let mut out = vec![];
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(bad("eof in headers"));
        }
        let l = line.trim_end();
        if l.is_empty() {
            return Ok(out);
        }
        if let Some((k, v)) = l.split_once(':') {
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
}

fn read_body<R: BufRead>(r: &mut R, headers: &[(String, String)], until_eof: bool) -> std::io::Result<Vec<u8>> {
    let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    if h("transfer-encoding").map(|v| v.to_ascii_lowercase().contains("chunked")).unwrap_or(false) {
        let mut body = vec![];
        loop {
            let mut line = String::new();
            r.read_line(&mut line)?;
            let size_s = line.trim().split(';').next().unwrap_or("");
            let size = usize::from_str_radix(size_s, 16).map_err(|_| bad("bad chunk size"))?;
            if size == 0 {
                // trailers
                loop {
                    let mut t = String::new();
                    if r.read_line(&mut t)? == 0 || t.trim().is_empty() {
                        break;
                    }
                }
                return Ok(body);
            }
            let mut chunk = vec![0u8; size];
            r.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            let mut crlf = String::new();
            r.read_line(&mut crlf)?;
        }
    } else if let Some(len) = h("content-length") {
        let n: usize = len.parse().map_err(|_| bad("bad content-length"))?;
        let mut body = vec![0u8; n];
        r.read_exact(&mut body)?;
        Ok(body)
    } else if until_eof {
        let mut body = vec![];
        r.read_to_end(&mut body)?;
        Ok(body)
    } else {
        Ok(vec![])
    }
}

fn read_response<R: BufRead>(r: &mut R) -> std::io::Result<Response> {
    let mut status_line = String::new();
    if r.read_line(&mut status_line)? == 0 {
        return Err(bad("empty response"));
    }
    let mut parts = status_line.split_whitespace();
    let _ver = parts.next();
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| bad("bad status line"))?;
    let headers = read_headers(r)?;
    let body = read_body(r, &headers, true)?;
    Ok(Response { status, body })
}

/// A parsed inbound request (server side).
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// Serve requests forever on `listener`, one thread per connection.
pub fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(&Request) -> (u16, String) + Send + Sync + 'static,
{
    let handler = std::sync::Arc::new(handler);
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let handler = handler.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut w = match stream.try_clone() {
                Ok(w) => w,
                Err(_) => return,
            };
            let mut r = BufReader::new(stream);
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut p = line.split_whitespace();
            let method = p.next().unwrap_or("").to_string();
            let path = p.next().unwrap_or("").to_string();
            let Ok(headers) = read_headers(&mut r) else { return };
            let body = read_body(&mut r, &headers, false).unwrap_or_default();
            let (status, resp) = handler(&Request { method, path, body });
            let out = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                resp.len()
            );
            let _ = w.write_all(out.as_bytes());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_tiny_server() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            serve(l, |req| (200, format!("{{\"m\":\"{}\",\"p\":\"{}\",\"n\":{}}}", req.method, req.path, req.body.len())))
        });
        let r = get(&addr, "/admin/status", Duration::from_secs(2)).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), r#"{"m":"GET","p":"/admin/status","n":0}"#);
        let r = post_json(&addr, "/admin/chaos", "{}", Duration::from_secs(2)).unwrap();
        assert_eq!(r.text(), r#"{"m":"POST","p":"/admin/chaos","n":2}"#);
    }

    #[test]
    fn chunked_body_is_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let r = read_response(&mut BufReader::new(&raw[..])).unwrap();
        assert_eq!(r.body, b"abcde");
    }

    #[test]
    fn connection_refused_is_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        assert!(get(&addr, "/", Duration::from_millis(300)).is_err());
    }
}
