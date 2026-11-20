//! Minimal, strict HTTP/1.1 request parser and response writer for the collector service.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::TcpStream,
};

const MAX_REQUEST_LINE_BYTES: usize = 2048;
const MAX_HEADER_COUNT: usize = 64;
const MAX_HEADER_LINE_BYTES: usize = 4096;

#[derive(Debug)]
pub struct HttpRequestHead {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
}

impl HttpRequestHead {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

#[derive(Debug)]
pub enum ParseError {
    BadRequest(&'static str),
    Io,
}

impl From<io::Error> for ParseError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

/// Reads the HTTP/1.1 request line and headers from `reader` with strict size bounds.
///
/// # Errors
/// Returns `ParseError::BadRequest` for malformed or oversized headers, or `ParseError::Io` on socket error.
pub fn read_request_head(
    reader: &mut BufReader<&mut TcpStream>,
) -> Result<HttpRequestHead, ParseError> {
    let mut line = String::new();
    let read = read_bounded_line(reader, &mut line, MAX_REQUEST_LINE_BYTES)?;
    if read == 0 {
        return Err(ParseError::BadRequest("empty request"));
    }
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let mut parts = trimmed.split_whitespace();
    let method = parts
        .next()
        .ok_or(ParseError::BadRequest("missing method"))?
        .to_owned();
    let target = parts
        .next()
        .ok_or(ParseError::BadRequest("missing target"))?;
    let version = parts
        .next()
        .ok_or(ParseError::BadRequest("missing version"))?;
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(ParseError::BadRequest("unsupported HTTP version"));
    }
    let path = target.split('?').next().unwrap_or(target).to_owned();

    let mut headers = HashMap::new();
    for _ in 0..MAX_HEADER_COUNT {
        line.clear();
        let bytes = read_bounded_line(reader, &mut line, MAX_HEADER_LINE_BYTES)?;
        if bytes == 0 {
            return Err(ParseError::BadRequest("unexpected EOF in headers"));
        }
        let header_line = line.trim_end_matches(['\r', '\n']);
        if header_line.is_empty() {
            return Ok(HttpRequestHead {
                method,
                path,
                headers,
            });
        }
        let Some((name, value)) = header_line.split_once(':') else {
            return Err(ParseError::BadRequest("malformed header"));
        };
        let key = name.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(ParseError::BadRequest("empty header name"));
        }
        headers.insert(key, value.trim().to_owned());
    }
    Err(ParseError::BadRequest("too many headers"))
}

/// Reads an exact `content_length` body into a vector.
///
/// # Errors
/// Returns I/O errors if the client closes early or times out.
pub fn read_body(
    reader: &mut BufReader<&mut TcpStream>,
    content_length: usize,
) -> io::Result<Vec<u8>> {
    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(body)
}

/// Writes an HTTP/1.1 response with `Connection: close`.
///
/// # Errors
/// Returns socket write errors.
pub fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body}",
        body.len()
    )?;
    stream.flush()
}

fn read_bounded_line(
    reader: &mut BufReader<&mut TcpStream>,
    buf: &mut String,
    limit: usize,
) -> Result<usize, ParseError> {
    let mut total = 0_usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total);
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |idx| idx + 1);
        if total + take > limit {
            return Err(ParseError::BadRequest("header line too long"));
        }
        let chunk = std::str::from_utf8(&available[..take])
            .map_err(|_| ParseError::BadRequest("non-utf8 header"))?;
        buf.push_str(chunk);
        total += take;
        reader.consume(take);
        if newline.is_some() {
            return Ok(total);
        }
    }
}
