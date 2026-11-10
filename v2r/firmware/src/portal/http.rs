//! Just enough HTTP/1.1 for the portal: one request per connection, a form in
//! the query or an urlencoded body, and a whole response.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_net::tcp::TcpSocket;

const MAX_REQUEST: usize = 3072;

pub struct Request {
    buffer: Vec<u8>,
    method_end: usize,
    path: (usize, usize),
    query: (usize, usize),
    host: (usize, usize),
    body_start: usize,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn hex(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// `application/x-www-form-urlencoded`, decoded.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

impl Request {
    fn text(&self, (start, end): (usize, usize)) -> &str {
        core::str::from_utf8(&self.buffer[start..end]).unwrap_or("")
    }

    pub fn method(&self) -> &str {
        self.text((0, self.method_end))
    }

    pub fn path(&self) -> &str {
        self.text(self.path)
    }

    /// The Host header, without the port.
    pub fn host(&self) -> &str {
        self.text(self.host)
    }

    fn body(&self) -> &str {
        core::str::from_utf8(&self.buffer[self.body_start..]).unwrap_or("")
    }

    /// A form field, from the query or the body, as `server->arg` finds it.
    pub fn arg(&self, name: &str) -> Option<String> {
        [self.text(self.query), self.body()]
            .into_iter()
            .flat_map(|form| form.split('&'))
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(key) == name).then(|| decode(value))
            })
            .next()
    }

    pub fn has_arg(&self, name: &str) -> bool {
        self.arg(name).is_some()
    }
}

/// Reads one request: its head, then as much body as Content-Length says.
pub async fn read(socket: &mut TcpSocket<'_>) -> Option<Request> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    let head_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() >= MAX_REQUEST {
            return None;
        }
        let count = socket.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..count]);
    };
    let head = core::str::from_utf8(&buffer[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let start = lines.next()?;
    let method_end = start.find(' ')?;
    let target_start = method_end + 1;
    let target_end = target_start + start[target_start..].find(' ').unwrap_or(start.len() - target_start);
    let target = &start[target_start..target_end];
    let (path, query) = match target.find('?') {
        Some(at) => ((target_start, target_start + at), (target_start + at + 1, target_end)),
        None => ((target_start, target_end), (target_end, target_end)),
    };
    let mut host = (0, 0);
    let mut length = 0usize;
    let mut offset = start.len() + 2;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value_start = offset + name.len() + 1 + (value.len() - value.trim_start().len());
            let value = value.trim();
            if name.eq_ignore_ascii_case("host") {
                let value = value.split(':').next().unwrap_or("");
                host = (value_start, value_start + value.len());
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            }
        }
        offset += line.len() + 2;
    }
    let body_start = head_end + 4;
    let wanted = (body_start + length).min(MAX_REQUEST);
    while buffer.len() < wanted {
        let count = socket.read(&mut chunk).await.ok()?;
        if count == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
    buffer.truncate(wanted.max(body_start).min(buffer.len()));
    Some(Request { buffer, method_end, path, query, host, body_start })
}

async fn write_all(socket: &mut TcpSocket<'_>, mut bytes: &[u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        let written = socket.write(bytes).await.map_err(|_| ())?;
        if written == 0 {
            return Err(());
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

/// Sends a whole response and closes the connection.
pub async fn respond(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(), ()> {
    let mut head = String::with_capacity(256);
    head.push_str("HTTP/1.1 ");
    head.push_str(status);
    head.push_str("\r\nContent-Type: ");
    head.push_str(content_type);
    for (name, value) in headers {
        head.push_str("\r\n");
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
    }
    head.push_str("\r\nContent-Length: ");
    head.push_str(itoa(body.len()).as_str());
    head.push_str("\r\nConnection: close\r\n\r\n");
    write_all(socket, head.as_bytes()).await?;
    write_all(socket, body.as_bytes()).await?;
    socket.flush().await.map_err(|_| ())
}

pub fn itoa(value: usize) -> heapless::String<20> {
    let mut out = heapless::String::new();
    let _ = core::fmt::write(&mut out, format_args!("{value}"));
    out
}
