// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Just enough HTTP/1.1 for the runner page: one request per connection,
//! read with hard limits, answered and closed.
//!
//! The page is served to a browser on the same PC (see ui.rs), so this
//! needs none of what a public web server does; it saves a dependency and
//! keeps everything the page can reach in one short file.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The most a request's line and headers may hold.
const HEAD_LIMIT: usize = 16 * 1024;
/// The most a request's body may hold: the page sends small JSON.
const BODY_LIMIT: usize = 64 * 1024;
/// How long a browser may take to send its request.
const READ_TIME: Duration = Duration::from_secs(10);

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
    let reason = match response.status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        421 => "Misdirected Request",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&response.body));
    let _ = stream.flush();
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
}
