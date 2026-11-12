// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Just enough HTTP/1.1 for the runner page and real machines: one
//! request per connection, read with hard limits, answered and closed.
//!
//! The page is served to a browser on the same PC (see ui.rs), and the
//! net-boot server to a machine on the lab's own network (see
//! netboot.rs), so this needs none of what a public web server does; it
//! saves a dependency and keeps everything they can reach in one short
//! file. [`send`] is the other side, for talking to Intel AMT (see
//! wsman.rs).

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// The most a request's line and headers may hold.
const HEAD_LIMIT: usize = 16 * 1024;
/// The most a request's body may hold: the page sends small JSON.
const BODY_LIMIT: usize = 64 * 1024;
/// How long a browser may take to send its request.
const READ_TIME: Duration = Duration::from_secs(10);
/// How long [`send`] waits between tries while a server refuses.
const REFUSED_GAP: Duration = Duration::from_millis(500);

/// A request, read.
#[derive(Debug, Default)]
pub struct Request {
    pub method: String,
    /// The path, decoded, without the query.
    pub path: String,
    /// The query's fields, decoded, in order.
    pub query: Vec<(String, String)>,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// The value of cookie `name`, if the request carries it.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.header("cookie")?
            .split(';')
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }
}

/// An answer.
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response { status, content_type, headers: Vec::new(), body: body.into() }
    }

    pub fn text(status: u16, text: &str) -> Response {
        Response::new(status, "text/plain; charset=utf-8", text)
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        Response::new(status, "application/json", value.to_string())
    }

    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Response {
        self.headers.push((name, value.into()));
        self
    }
}

/// Reads one request from `stream`.
pub fn read_request(stream: &TcpStream) -> Result<Request, String> {
    stream.set_read_timeout(Some(READ_TIME)).map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(stream.take((HEAD_LIMIT + BODY_LIMIT) as u64));
    parse(&mut reader)
}

/// Reads a request's head and body from `reader`.
fn parse(reader: &mut impl BufRead) -> Result<Request, String> {
    let mut head = 0;
    let mut line = String::new();
    let mut next_line = |line: &mut String| -> Result<(), String> {
        line.clear();
        let read = reader.read_line(line).map_err(|err| err.to_string())?;
        head += read;
        if read == 0 || head > HEAD_LIMIT {
            return Err("the request's head is cut short or too long".into());
        }
        Ok(())
    };
    next_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(_version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err("a bad request line".into());
    };
    let mut request = Request { method: method.to_string(), ..Request::default() };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    request.path = decode(path);
    request.query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect();
    loop {
        next_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("a bad header line")?;
        request.headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let length: usize = match request.header("content-length") {
        Some(length) => length.parse().map_err(|_| "a bad Content-Length")?,
        None => 0,
    };
    if length > BODY_LIMIT {
        return Err("the request's body is too long".into());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|err| format!("reading the body: {err}"))?;
    request.body = body;
    Ok(request)
}

/// `text` with `%xx` escapes and `+` decoded; bytes that do not make
/// UTF-8 become U+FFFD.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes `response` to `stream` and closes the connection.
pub fn write_response(mut stream: &TcpStream, response: &Response) {
    let mut head = head(response.status, response.content_type, response.body.len() as u64);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&response.body));
    let _ = stream.flush();
}

/// Writes an answer's head, for a body of `length` bytes the caller
/// writes after it, as it reads them: a file too big to hold at once.
pub fn write_head(
    mut stream: &TcpStream,
    status: u16,
    content_type: &str,
    length: u64,
) -> std::io::Result<()> {
    let mut head = head(status, content_type, length);
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

/// An answer's status line and the headers every answer has.
fn head(status: u16, content_type: &str, length: u64) -> String {
    let reason = match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        421 => "Misdirected Request",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n"
    )
}

/// The most an answer to [`send`] may hold: AMT's longest, a list of its
/// firmware's parts, is tens of kilobytes.
const ANSWER_LIMIT: usize = 4 << 20;

/// What a server answered to [`send`].
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

/// Sends one request to `address` (`host:port`) and reads the whole
/// answer, giving up when connecting or any read takes longer than
/// `time`. A server that refuses the connection outright is tried again
/// every half second for up to `patience`: Intel AMT refuses its first
/// connection for a moment when its engine has dozed off with the
/// machine powered off, and answers once that connection has woken it.
pub fn send(
    address: &str,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
    time: Duration,
    patience: Duration,
) -> Result<Answer, String> {
    let to = address
        .to_socket_addrs()
        .map_err(|err| format!("{address}: {err}"))?
        .next()
        .ok_or_else(|| format!("{address}: no address"))?;
    let deadline = Instant::now() + patience;
    let mut stream = loop {
        match TcpStream::connect_timeout(&to, time) {
            Ok(stream) => break stream,
            Err(err) if err.kind() == ErrorKind::ConnectionRefused && Instant::now() < deadline => {
                std::thread::sleep(REFUSED_GAP);
            }
            Err(err) => return Err(format!("connecting to {address}: {err}")),
        }
    };
    stream.set_read_timeout(Some(time)).map_err(|err| err.to_string())?;
    stream.set_write_timeout(Some(time)).map_err(|err| err.to_string())?;
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .map_err(|err| format!("sending to {address}: {err}"))?;
    read_answer(&mut BufReader::new(stream.take(ANSWER_LIMIT as u64 + 64 * 1024)))
        .map_err(|err| format!("reading {address}'s answer: {err}"))
}

/// Reads an answer: its head, then a body of Content-Length bytes, in
/// chunks, or up to the end of the connection.
fn read_answer(reader: &mut impl BufRead) -> Result<Answer, String> {
    let mut line = String::new();
    let mut next_line = |line: &mut String| -> Result<(), String> {
        line.clear();
        let read = reader.read_line(line).map_err(|err| err.to_string())?;
        if read == 0 || line.len() > HEAD_LIMIT {
            return Err("the answer is cut short".into());
        }
        Ok(())
    };
    next_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| format!("a bad status line: {}", line.trim()))?;
    let mut headers = Vec::new();
    loop {
        next_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("a bad header line")?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let mut answer = Answer { status, headers, body: Vec::new() };
    let chunked = answer
        .header("transfer-encoding")
        .is_some_and(|coding| coding.to_ascii_lowercase().contains("chunked"));
    if chunked {
        answer.body = read_chunks(reader)?;
    } else if let Some(length) = answer.header("content-length") {
        let length: usize = length.parse().map_err(|_| "a bad Content-Length")?;
        if length > ANSWER_LIMIT {
            return Err("the answer is too long".into());
        }
        answer.body = vec![0; length];
        reader.read_exact(&mut answer.body).map_err(|err| err.to_string())?;
    } else {
        reader.read_to_end(&mut answer.body).map_err(|err| err.to_string())?;
    }
    Ok(answer)
}

/// Reads a chunked body, and the trailer after it.
fn read_chunks(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).map_err(|err| err.to_string())?;
        let size = line.trim().split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size, 16)
            .map_err(|_| format!("a bad chunk size: {}", line.trim()))?;
        if size == 0 {
            break;
        }
        if body.len() + size > ANSWER_LIMIT {
            return Err("the answer is too long".into());
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).map_err(|err| err.to_string())?;
        line.clear();
        reader.read_line(&mut line).map_err(|err| err.to_string())?;
    }
    // The trailer, ended by an empty line.
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|err| err.to_string())?;
        if read == 0 || line.trim().is_empty() {
            return Ok(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_request() {
        let raw = "POST /api/run?x=a%20b&y=1+2 HTTP/1.1\r\nHost: 127.0.0.1:7420\r\nCookie: a=1; dukovo_lab=t0k\r\nContent-Length: 2\r\n\r\n{}";
        let request = parse(&mut raw.as_bytes()).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/run");
        assert_eq!(request.query("x"), Some("a b"));
        assert_eq!(request.query("y"), Some("1 2"));
        assert_eq!(request.header("host"), Some("127.0.0.1:7420"));
        assert_eq!(request.cookie("dukovo_lab"), Some("t0k"));
        assert_eq!(request.body, b"{}");
    }

    #[test]
    fn refuses_what_is_too_long_or_cut_short() {
        let long = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(HEAD_LIMIT));
        assert!(parse(&mut long.as_bytes()).is_err());
        assert!(parse(&mut "GET / HTTP/1.1\r\nHost: x".as_bytes()).is_err());
        let big = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", BODY_LIMIT + 1);
        assert!(parse(&mut big.as_bytes()).is_err());
        assert_eq!(decode("%2e%2E%2fx%"), "../x%");
        assert_eq!(decode("%zz"), "%zz");
    }

    #[test]
    fn waits_for_a_server_that_refuses_while_it_wakes() {
        // A port nothing listens on: refused at once.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let address = format!("127.0.0.1:{port}");
        let time = Duration::from_secs(5);
        let refused = send(&address, "GET", "/", &[], &[], time, Duration::ZERO).unwrap_err();
        assert!(refused.starts_with(&format!("connecting to {address}")), "{refused}");
        // A server that starts listening a moment later is waited for.
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            parse(&mut BufReader::new(&stream)).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nup").unwrap();
        });
        let answer = send(&address, "GET", "/", &[], &[], time, Duration::from_secs(20)).unwrap();
        assert_eq!((answer.status, answer.body.as_slice()), (200, &b"up"[..]));
        server.join().unwrap();
    }

    #[test]
    fn reads_answers_of_each_kind() {
        let sized = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: b\r\n\r\nhello and more";
        let answer = read_answer(&mut sized.as_bytes()).unwrap();
        assert_eq!((answer.status, answer.body.as_slice()), (200, &b"hello"[..]));
        assert_eq!(answer.header("x-a"), Some("b"));
        let chunked = "HTTP/1.1 401 Unauthorized\r\nTransfer-Encoding: chunked\r\n\r\n\
                       4\r\nWiki\r\n6;x=y\r\npedia \r\n0\r\nX-T: 1\r\n\r\n";
        let answer = read_answer(&mut chunked.as_bytes()).unwrap();
        assert_eq!((answer.status, answer.body.as_slice()), (401, &b"Wikipedia "[..]));
        let to_close = "HTTP/1.0 200 OK\r\n\r\nall of it";
        assert_eq!(read_answer(&mut to_close.as_bytes()).unwrap().body, b"all of it");
        assert!(
            read_answer(&mut "HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort".as_bytes())
                .is_err()
        );
        assert!(read_answer(&mut "nonsense\r\n\r\n".as_bytes()).is_err());
    }
}
