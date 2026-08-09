//! A hand-written HTTP server on the loopback interface.
//!
//! The download is tested against a real socket rather than a stubbed-out
//! client trait: a trait with one implementation is what AGENTS.md tells us not
//! to write, and it would only ever prove that our own stub behaves the way we
//! imagined. A socket exercises our code and `ureq` together.

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
}

pub struct TestServer {
    url: String,
    ranges: Arc<Mutex<Vec<Option<String>>>>,
}

impl TestServer {
    pub fn start(body: Vec<u8>, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the loopback port should bind");
        let url = format!(
            "http://{}/noble-cloudimg-amd64.img",
            listener.local_addr().unwrap()
        );
        let ranges = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&ranges);
        // The thread is left to park in `accept` when the test ends: the test
        // binary exits and takes it with it, which is cheaper than wiring a
        // shutdown protocol into a fixture.
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let range = answer(stream, &body, behaviour);
                recorded.lock().unwrap().push(range);
            }
        });

        Self { url, ranges }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The `range` header of every request served so far, in order.
    pub fn ranges_seen(&self) -> Vec<Option<String>> {
        self.ranges.lock().unwrap().clone()
    }
}

fn answer(mut stream: TcpStream, body: &[u8], behaviour: Behaviour) -> Option<String> {
    let range = read_range_header(&stream);
    let requested_from = range
        .as_deref()
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<usize>().ok());

    match (behaviour, requested_from) {
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

    range
}

/// Reads the request and returns its `range` header, if any.
///
/// Header names are matched case-insensitively because `ureq` sends them
/// lowercase (`range: bytes=1000-`). A server looking for `Range: ` would
/// silently answer 200 to every resume, and the resume test would pass while
/// testing nothing.
fn read_range_header(stream: &TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().expect("the stream should clone"));
    let mut range = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_owned());
        }
    }
    range
}
