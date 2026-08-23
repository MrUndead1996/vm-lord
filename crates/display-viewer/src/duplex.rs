//! Two ends of a socket, in memory.
//!
//! Reads answer `WouldBlock` when there is nothing yet, which is what the
//! record reader turns into `RecordError::Idle` -- the same thing a bounded
//! HvSocket read reports when the peer is simply quiet. That is what lets a
//! test drive the same loop the window does.

use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{Arc, Mutex},
};

/// One end of an in-memory socket.
pub struct Duplex {
    incoming: Arc<Mutex<VecDeque<u8>>>,
    outgoing: Arc<Mutex<VecDeque<u8>>>,
}

/// A connected pair.
#[must_use]
pub fn pair() -> (Duplex, Duplex) {
    let left = Arc::new(Mutex::new(VecDeque::new()));
    let right = Arc::new(Mutex::new(VecDeque::new()));

    (
        Duplex {
            incoming: Arc::clone(&left),
            outgoing: Arc::clone(&right),
        },
        Duplex {
            incoming: right,
            outgoing: left,
        },
    )
}

impl Read for Duplex {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut incoming = self.incoming.lock().expect("no test panics holding it");
        // A quiet socket, never a closed one: nothing in these tests hangs up,
        // and `WouldBlock` is what a bounded HvSocket read reports meanwhile.
        if incoming.is_empty() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }

        let mut read = 0;
        while read < buffer.len() {
            match incoming.pop_front() {
                Some(byte) => {
                    buffer[read] = byte;
                    read += 1;
                }
                None => break,
            }
        }

        Ok(read)
    }
}

impl Write for Duplex {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.outgoing
            .lock()
            .expect("no test panics holding it")
            .extend(buffer);

        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
