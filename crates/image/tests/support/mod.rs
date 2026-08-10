//! A hand-written HTTP server on the loopback interface.
//!
//! The download is tested against a real socket rather than a stubbed-out
//! client trait: a trait with one implementation is what AGENTS.md tells us not
//! to write, and it would only ever prove that our own stub behaves the way we
//! imagined. A socket exercises our code and `ureq` together.

// Each integration test binary compiles this module separately, and none of
// them uses every behaviour, so unused-code warnings here mean nothing.
#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

/// How the server answers a request.
#[derive(Clone, Copy, Debug)]
pub enum Behaviour {
    /// Honours `range`, answering 206 when one is asked for.
    Ranged,
    /// Answers 200 with the whole body whatever `range` says.
    IgnoresRange,
    /// Answers 416 to any `range`, and 200 to a request without one.
    RejectsRange,
    /// Promises the whole length, sends `bytes`, then hangs up.
    Truncated { bytes: usize },
    /// Has no such image.
    NotFound,
}

pub struct TestServer {
    url: String,
    base_url: String,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
}

impl TestServer {
    pub fn start(body: Vec<u8>, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback port should bind");
        let url = format!(
            "http://{}/noble-cloudimg-amd64.img",
            listener.local_addr().unwrap()
        );
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&ranges);
        // The thread is left to park in `accept` when the test ends: the test
        // binary exits and takes it with it, which is cheaper than wiring a
        // shutdown protocol into a fixture.
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let range = read_range_header(&stream);
                answer(stream, range.as_deref(), &body, behaviour);
                recorded.lock().unwrap().push(range);
            }
        });

        Self {
            url,
            base_url,
            ranges,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The server's root, ending in a slash: the directory a profile points at.
    ///
    /// The server answers every path the same way, so a resolver asking for
    /// `<base>/SHA256SUMS` gets the body the test handed in.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The `range` header of every request served so far, in order.
    pub fn ranges_seen(&self) -> Vec<Option<String>> {
        self.ranges.lock().unwrap().clone()
    }

    /// Serves a directory: each request is answered with the file whose name it
    /// asks for, and with 404 when there is no such file.
    ///
    /// `start` above answers every path with one body, which is enough for a
    /// resolver or a download on its own but not for `open_cloud_image`, which
    /// fetches the checksum list and the image it names in one call.
    pub fn start_directory(files: Vec<(String, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback port should bind");
        let url = format!(
            "http://{}/noble-cloudimg-amd64.img",
            listener.local_addr().unwrap()
        );
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&ranges);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let (range, path) = read_request(&stream);
                let file = files
                    .iter()
                    .find(|(name, _)| path.rsplit('/').next() == Some(name.as_str()));
                match file {
                    Some((_, body)) => {
                        answer(stream, range.as_deref(), body, Behaviour::IgnoresRange)
                    }
                    None => answer(stream, range.as_deref(), &[], Behaviour::NotFound),
                };
                recorded.lock().unwrap().push(range);
            }
        });

        Self {
            url,
            base_url,
            ranges,
        }
    }
}

/// Writes a response for a request whose `range` header has already been read.
///
/// The header is read once, by the caller, and handed in rather than read
/// again here: a second read on the same connection has nothing left to read
/// -- the client sent the request in one write and is now blocked waiting on
/// the response -- and blocks until the client's own read timeout fires.
fn answer(mut stream: TcpStream, range: Option<&str>, body: &[u8], behaviour: Behaviour) {
    let requested_from = range
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<usize>().ok());

    match (behaviour, requested_from) {
        (Behaviour::NotFound, _) => {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            let _ = stream.flush();
        }
        (Behaviour::Truncated { bytes }, _) => {
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body[..bytes.min(body.len())]);
            let _ = stream.flush();
            // Dropping the stream here is the point: the client sees EOF with
            // bytes still outstanding.
        }
        (Behaviour::RejectsRange, Some(_)) => {
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
        }
        (Behaviour::Ranged, Some(from)) if from < body.len() => {
            let slice = &body[from..];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                slice.len(),
                from,
                body.len() - 1,
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(slice);
            let _ = stream.flush();
        }
        _ => {
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
    }
}

/// Reads the request, returning its `range` header and its path.
///
/// Header names are matched case-insensitively because `ureq` sends them
/// lowercase (`range: bytes=1000-`). A server looking for `Range: ` would
/// silently answer 200 to every resume, and the resume test would pass while
/// testing nothing.
fn read_request(stream: &TcpStream) -> (Option<String>, String) {
    let mut reader = BufReader::new(stream.try_clone().expect("the stream should clone"));
    let mut range = None;
    let mut path = String::new();
    let mut first = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" {
            break;
        }
        if first {
            first = false;
            path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_owned());
        }
    }
    (range, path)
}

fn read_range_header(stream: &TcpStream) -> Option<String> {
    read_request(stream).0
}
