//! One file transfer in each direction, and every rule about how they end.
//!
//! Like [`crate::clipboard::Exchange`], both ends run this and neither trusts
//! the other to have run it: a receiver that enforced no size limit would be a
//! disk a peer can fill, and a sender that enforced none would be a peer that
//! can be asked to read a terabyte. Nothing here opens a file, walks a
//! directory or knows a source path -- it says what to create and what to
//! write, and the platform adapters do it.

use std::{collections::HashSet, time::Instant};

use crate::{
    clipboard::{CHUNK, IDLE, path::ValidatedPath},
    v1::{FileCancelReason, FileEntryKind},
};

/// The most entries one tree may have, directories included.
///
/// A protocol constant rather than a setting: it bounds the bookkeeping this
/// machine does per transfer, which is not something a user should be able to
/// raise on either side of a session.
pub const MAX_ENTRIES: usize = 4096;

/// The default per-file limit, which is exactly one GiB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// The default per-transfer limit, which is exactly four GiB.
pub const DEFAULT_MAX_TRANSFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The default retention of a staged tree, which is a day.
pub const DEFAULT_RETENTION_SECONDS: u64 = 24 * 60 * 60;

/// What one entry of a tree is.
///
/// Nothing else crosses. A symlink, socket, FIFO or device ends the transfer
/// at the sender rather than arriving as something the receiver has to decide
/// about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file, whose bytes follow as chunks.
    File,
    /// A directory, which carries no bytes.
    Directory,
}

impl EntryKind {
    /// The kind a wire value names, if it names one at all.
    #[must_use]
    pub fn from_wire(value: i32) -> Option<Self> {
        match FileEntryKind::try_from(value) {
            Ok(FileEntryKind::File) => Some(Self::File),
            Ok(FileEntryKind::Directory) => Some(Self::Directory),
            _ => None,
        }
    }

    /// What this kind is on the wire.
    #[must_use]
    pub fn as_wire(self) -> i32 {
        match self {
            Self::File => FileEntryKind::File as i32,
            Self::Directory => FileEntryKind::Directory as i32,
        }
    }
}

/// The limits a session enforces, in the units the wire carries.
///
/// The host parses these from human-readable settings and announces them; both
/// ends then hold the narrower of what they were told and what they hold
/// themselves, so neither side depends on the other having been honest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    max_file_bytes: u64,
    max_transfer_bytes: u64,
    retention_seconds: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_FILE_BYTES,
            DEFAULT_MAX_TRANSFER_BYTES,
            DEFAULT_RETENTION_SECONDS,
        )
    }
}

impl Policy {
    /// The limits as bytes and seconds.
    #[must_use]
    pub fn new(max_file_bytes: u64, max_transfer_bytes: u64, retention_seconds: u64) -> Self {
        Self {
            max_file_bytes,
            max_transfer_bytes,
            retention_seconds,
        }
    }

    /// The most one file may carry.
    #[must_use]
    pub fn max_file_bytes(self) -> u64 {
        self.max_file_bytes
    }

    /// The most one tree may carry.
    #[must_use]
    pub fn max_transfer_bytes(self) -> u64 {
        self.max_transfer_bytes
    }

    /// How long a staged tree outlives the transfer that made it.
    #[must_use]
    pub fn retention_seconds(self) -> u64 {
        self.retention_seconds
    }

    /// The stricter of two policies, field by field.
    #[must_use]
    pub fn narrowed(self, other: Self) -> Self {
        Self {
            max_file_bytes: self.max_file_bytes.min(other.max_file_bytes),
            max_transfer_bytes: self.max_transfer_bytes.min(other.max_transfer_bytes),
            retention_seconds: self.retention_seconds.min(other.retention_seconds),
        }
    }
}

/// A message this machine puts on the clipboard channel.
///
/// The wire forms are the `ClipboardFile*` messages; encoding them is the
/// caller's, which is what keeps the schema's types out of this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// The limits this side holds, sent before it offers anything.
    Policy(Policy),
    /// My selection has files in it.
    Offer {
        /// Names the selection.
        serial: u32,
    },
    /// Send me that selection as a tree.
    Request {
        /// The offer this answers.
        serial: u32,
        /// Names the transfer that follows.
        transfer: u32,
    },
    /// One entry of the tree, before any of its bytes.
    Entry {
        /// The transfer it belongs to.
        transfer: u32,
        /// Its path, relative to the root of the tree.
        path: String,
        /// What it is.
        kind: EntryKind,
        /// How long a regular file is, and zero for a directory.
        size: u64,
    },
    /// The next bytes of the entry that is open, never logged.
    Chunk {
        /// The transfer it belongs to.
        transfer: u32,
        /// The bytes.
        chunk: Vec<u8>,
    },
    /// The whole tree passed the sender's checks and is now the receiver's.
    Complete {
        /// The transfer that finished.
        transfer: u32,
    },
    /// That transfer is over and the rest of the tree is not coming.
    Cancel {
        /// The transfer that ended.
        transfer: u32,
        /// Why it did.
        reason: FileCancelReason,
    },
}

/// What the caller must do, beyond writing records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Put this message on the clipboard channel.
    Send(Message),
    /// Walk the local selection and feed it back through
    /// [`Exchange::produced_entry`] and [`Exchange::produced_chunk`].
    Enumerate {
        /// The transfer being served.
        transfer: u32,
    },
    /// Create this entry under the transfer's staging root.
    CreateEntry {
        /// The transfer being staged.
        transfer: u32,
        /// Where it goes, already checked against every path rule.
        path: ValidatedPath,
        /// What to create.
        kind: EntryKind,
        /// How long the file will be.
        size: u64,
    },
    /// Append these bytes to the entry that is open.
    WriteChunk {
        /// The transfer being staged.
        transfer: u32,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// The tree is whole: publish it as a local selection.
    Commit {
        /// The transfer that finished.
        transfer: u32,
    },
    /// The tree is not going to be whole: close it and delete what is there.
    Abort {
        /// The transfer that ended.
        transfer: u32,
    },
}

/// The entry whose bytes are in flight.
#[derive(Clone, Copy)]
struct Open {
    declared: u64,
    written: u64,
}

/// What a transfer has accounted for so far, on whichever side holds it.
#[derive(Default)]
struct Tree {
    entries: usize,
    total: u64,
    open: Option<Open>,
    keys: HashSet<String>,
}

impl Tree {
    /// Checks one entry against every limit, and opens it if it has bytes.
    fn admit(
        &mut self,
        path: &str,
        kind: EntryKind,
        size: u64,
        policy: Policy,
    ) -> Result<ValidatedPath, FileCancelReason> {
        if self.open.is_some() {
            // The previous file is short of what it declared, so the stream is
            // no longer where this entry says it is.
            return Err(FileCancelReason::IoFailed);
        }

        let parsed = ValidatedPath::parse(path).map_err(|_| FileCancelReason::InvalidPath)?;

        if kind == EntryKind::Directory && size != 0 {
            return Err(FileCancelReason::UnsafeEntry);
        }
        if self.entries >= MAX_ENTRIES {
            return Err(FileCancelReason::TooLarge);
        }
        if size > policy.max_file_bytes() {
            return Err(FileCancelReason::TooLarge);
        }
        let total = self
            .total
            .checked_add(size)
            .ok_or(FileCancelReason::TooLarge)?;
        if total > policy.max_transfer_bytes() {
            return Err(FileCancelReason::TooLarge);
        }
        if !self.keys.insert(parsed.windows_key().to_owned()) {
            return Err(FileCancelReason::UnsafeEntry);
        }

        self.entries += 1;
        self.total = total;
        if kind == EntryKind::File && size > 0 {
            self.open = Some(Open {
                declared: size,
                written: 0,
            });
        }

        Ok(parsed)
    }

    /// Accounts for bytes against the entry that is open.
    fn wrote(&mut self, len: u64) -> Result<(), FileCancelReason> {
        let Some(open) = self.open.as_mut() else {
            return Err(FileCancelReason::IoFailed);
        };

        open.written = open
            .written
            .checked_add(len)
            .ok_or(FileCancelReason::IoFailed)?;
        if open.written > open.declared {
            return Err(FileCancelReason::IoFailed);
        }
        if open.written == open.declared {
            self.open = None;
        }

        Ok(())
    }

    /// Whether every entry admitted so far has all of its bytes.
    fn settled(&self) -> bool {
        self.open.is_none()
    }
}

/// The transfer this side is serving.
#[derive(Default)]
struct Outgoing {
    serial: u32,
    transfer: Option<u32>,
    since: Option<Instant>,
    tree: Tree,
}

/// The transfer this side is staging.
#[derive(Default)]
struct Incoming {
    transfer: Option<u32>,
    since: Option<Instant>,
    tree: Tree,
}

/// One side of one session's file clipboard.
pub struct Exchange {
    policy: Policy,
    announced: bool,
    heard: bool,
    outgoing: Outgoing,
    incoming: Incoming,
    /// The next transfer id this side names. Ids are chosen by whichever side
    /// asks, so the two directions never collide by construction.
    next_transfer: u32,
}

impl Exchange {
    /// A file clipboard with nothing in flight, holding these limits.
    #[must_use]
    pub fn new(policy: Policy, now: Instant) -> Self {
        Self {
            policy,
            announced: false,
            heard: false,
            outgoing: Outgoing {
                since: Some(now),
                ..Outgoing::default()
            },
            incoming: Incoming {
                since: Some(now),
                ..Incoming::default()
            },
            next_transfer: 1,
        }
    }

    /// The limits in force, which is the narrower of both sides'.
    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// The peer stated its limits.
    pub fn peer_policy(&mut self, policy: Policy) {
        self.policy = self.policy.narrowed(policy);
        self.heard = true;
    }

    /// The local selection has files in it.
    ///
    /// Like the in-memory clipboard, this costs one record however large the
    /// selection is: nothing is walked or read until the peer asks.
    pub fn local_offer(&mut self, now: Instant) -> Vec<Op> {
        let mut ops = Vec::new();
        if !self.announced {
            self.announced = true;
            ops.push(Op::Send(Message::Policy(self.policy)));
        }
        ops.extend(self.cancel_outgoing(FileCancelReason::Superseded));

        self.outgoing.serial = self.outgoing.serial.wrapping_add(1);
        self.outgoing.since = Some(now);
        ops.push(Op::Send(Message::Offer {
            serial: self.outgoing.serial,
        }));

        ops
    }

    /// The peer's selection has files in it.
    ///
    /// An offer from a peer that has not stated its limits is left alone: the
    /// policy is applied before an offer is accepted, not after a tree has
    /// begun to arrive.
    pub fn peer_offer(&mut self, serial: u32, now: Instant) -> Vec<Op> {
        if !self.heard {
            return Vec::new();
        }

        let mut ops = self.cancel_incoming(FileCancelReason::Superseded);

        let transfer = self.next_transfer;
        self.next_transfer = self.next_transfer.wrapping_add(1);
        self.incoming = Incoming {
            transfer: Some(transfer),
            since: Some(now),
            tree: Tree::default(),
        };
        ops.push(Op::Send(Message::Request { serial, transfer }));

        ops
    }

    /// The peer asks for the selection this side announced.
    pub fn peer_request(&mut self, serial: u32, transfer: u32, now: Instant) -> Vec<Op> {
        if serial != self.outgoing.serial {
            return vec![Op::Send(Message::Cancel {
                transfer,
                reason: FileCancelReason::Superseded,
            })];
        }

        let mut ops = self.cancel_outgoing(FileCancelReason::Superseded);
        self.outgoing.transfer = Some(transfer);
        self.outgoing.since = Some(now);
        self.outgoing.tree = Tree::default();
        ops.push(Op::Enumerate { transfer });

        ops
    }

    /// The local walk found an entry.
    pub fn produced_entry(
        &mut self,
        transfer: u32,
        path: &str,
        kind: EntryKind,
        size: u64,
        now: Instant,
    ) -> Vec<Op> {
        if self.outgoing.transfer != Some(transfer) {
            return Vec::new();
        }

        match self.outgoing.tree.admit(path, kind, size, self.policy) {
            Err(reason) => self.cancel_outgoing(reason),
            Ok(parsed) => {
                self.outgoing.since = Some(now);

                vec![Op::Send(Message::Entry {
                    transfer,
                    path: parsed.as_str().to_owned(),
                    kind,
                    size,
                })]
            }
        }
    }

    /// The local read produced the next bytes of the entry that is open.
    ///
    /// One call is one record, so the caller decides how long it stays here
    /// before returning to its socket, its clipboard and its focus events.
    pub fn produced_chunk(&mut self, transfer: u32, chunk: Vec<u8>, now: Instant) -> Vec<Op> {
        if self.outgoing.transfer != Some(transfer) {
            return Vec::new();
        }
        if chunk.len() > CHUNK {
            return self.cancel_outgoing(FileCancelReason::TooLarge);
        }

        match self.outgoing.tree.wrote(chunk.len() as u64) {
            Err(reason) => self.cancel_outgoing(reason),
            Ok(()) => {
                self.outgoing.since = Some(now);

                vec![Op::Send(Message::Chunk { transfer, chunk })]
            }
        }
    }

    /// The local walk reached the end of the tree.
    pub fn produced_complete(&mut self, transfer: u32, now: Instant) -> Vec<Op> {
        if self.outgoing.transfer != Some(transfer) {
            return Vec::new();
        }
        if !self.outgoing.tree.settled() {
            return self.cancel_outgoing(FileCancelReason::IoFailed);
        }

        self.outgoing.transfer = None;
        self.outgoing.since = Some(now);
        self.outgoing.tree = Tree::default();

        vec![Op::Send(Message::Complete { transfer })]
    }

    /// An entry of the tree this side is staging.
    pub fn peer_entry(
        &mut self,
        transfer: u32,
        path: &str,
        kind: EntryKind,
        size: u64,
        now: Instant,
    ) -> Vec<Op> {
        if self.incoming.transfer != Some(transfer) {
            return Vec::new();
        }

        match self.incoming.tree.admit(path, kind, size, self.policy) {
            Err(reason) => self.cancel_incoming(reason),
            Ok(parsed) => {
                self.incoming.since = Some(now);

                vec![Op::CreateEntry {
                    transfer,
                    path: parsed,
                    kind,
                    size,
                }]
            }
        }
    }

    /// The next bytes of the entry this side has open.
    pub fn peer_chunk(&mut self, transfer: u32, chunk: &[u8], now: Instant) -> Vec<Op> {
        if self.incoming.transfer != Some(transfer) {
            return Vec::new();
        }

        match self.incoming.tree.wrote(chunk.len() as u64) {
            Err(reason) => self.cancel_incoming(reason),
            Ok(()) => {
                self.incoming.since = Some(now);

                vec![Op::WriteChunk {
                    transfer,
                    bytes: chunk.to_vec(),
                }]
            }
        }
    }

    /// The peer says the tree is whole.
    ///
    /// Nothing incomplete is ever published, so a tree whose last file is
    /// short of its length is aborted rather than committed.
    pub fn peer_complete(&mut self, transfer: u32, now: Instant) -> Vec<Op> {
        if self.incoming.transfer != Some(transfer) {
            return Vec::new();
        }
        if !self.incoming.tree.settled() {
            return self.cancel_incoming(FileCancelReason::IoFailed);
        }

        self.incoming.transfer = None;
        self.incoming.since = Some(now);
        self.incoming.tree = Tree::default();

        vec![Op::Commit { transfer }]
    }

    /// The peer gave up on a transfer.
    pub fn peer_cancel(&mut self, transfer: u32, _reason: FileCancelReason) -> Vec<Op> {
        if self.incoming.transfer == Some(transfer) {
            self.forget_incoming();

            return vec![Op::Abort { transfer }];
        }

        if self.outgoing.transfer == Some(transfer) {
            self.forget_outgoing();
        }

        Vec::new()
    }

    /// The window lost focus, so no tree may cross in either direction.
    pub fn focus_lost(&mut self, _now: Instant) -> Vec<Op> {
        let mut ops = self.cancel_outgoing(FileCancelReason::FocusLost);
        ops.extend(self.cancel_incoming(FileCancelReason::FocusLost));

        ops
    }

    /// Cancels whichever transfer has stopped moving.
    pub fn tick(&mut self, now: Instant) -> Vec<Op> {
        let mut ops = Vec::new();

        if self.outgoing.transfer.is_some() && self.stalled(self.outgoing.since, now) {
            ops.extend(self.cancel_outgoing(FileCancelReason::TimedOut));
        }
        if self.incoming.transfer.is_some() && self.stalled(self.incoming.since, now) {
            ops.extend(self.cancel_incoming(FileCancelReason::TimedOut));
        }

        ops
    }

    /// Whether a transfer has made no progress for longer than it may.
    fn stalled(&self, since: Option<Instant>, now: Instant) -> bool {
        since.is_some_and(|since| now.duration_since(since) > IDLE)
    }

    /// Ends the transfer this side is serving, if there is one.
    fn cancel_outgoing(&mut self, reason: FileCancelReason) -> Vec<Op> {
        match self.outgoing.transfer {
            Some(transfer) => {
                self.forget_outgoing();

                vec![Op::Send(Message::Cancel { transfer, reason })]
            }
            None => Vec::new(),
        }
    }

    /// Ends the transfer this side is staging, and removes what it staged.
    fn cancel_incoming(&mut self, reason: FileCancelReason) -> Vec<Op> {
        match self.incoming.transfer {
            Some(transfer) => {
                self.forget_incoming();

                vec![
                    Op::Send(Message::Cancel { transfer, reason }),
                    Op::Abort { transfer },
                ]
            }
            None => Vec::new(),
        }
    }

    /// Drops what this side was serving, without saying anything about it.
    fn forget_outgoing(&mut self) {
        self.outgoing.transfer = None;
        self.outgoing.since = None;
        self.outgoing.tree = Tree::default();
    }

    /// Drops what this side was staging, without saying anything about it.
    fn forget_incoming(&mut self) {
        self.incoming.transfer = None;
        self.incoming.since = None;
        self.incoming.tree = Tree::default();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::clipboard::IDLE;

    /// Small enough to reach the limits without allocating anything.
    fn policy() -> Policy {
        Policy::new(1024, 4096, 3600)
    }

    fn now() -> Instant {
        Instant::now()
    }

    /// A receiver that knows the peer's limits and has asked for an offer.
    fn pulling(now: Instant) -> Exchange {
        let mut exchange = Exchange::new(policy(), now);
        exchange.peer_policy(policy());
        assert_eq!(
            exchange.peer_offer(7, now),
            vec![Op::Send(Message::Request {
                serial: 7,
                transfer: 1,
            })]
        );

        exchange
    }

    #[test]
    fn a_local_selection_states_the_limits_once_and_then_only_offers() {
        let now = now();
        let mut exchange = Exchange::new(policy(), now);

        assert_eq!(
            exchange.local_offer(now),
            vec![
                Op::Send(Message::Policy(policy())),
                Op::Send(Message::Offer { serial: 1 })
            ]
        );
        assert_eq!(
            exchange.local_offer(now),
            vec![Op::Send(Message::Offer { serial: 2 })]
        );
    }

    #[test]
    fn an_offer_from_a_peer_that_has_not_stated_its_limits_is_not_pulled() {
        let now = now();
        let mut exchange = Exchange::new(policy(), now);

        assert_eq!(exchange.peer_offer(7, now), Vec::new());
    }

    #[test]
    fn the_narrower_of_the_two_policies_is_what_binds() {
        let now = now();
        let mut exchange = Exchange::new(policy(), now);
        exchange.peer_policy(Policy::new(512, 8192, 60));

        assert_eq!(exchange.policy(), Policy::new(512, 4096, 60));
    }

    #[test]
    fn a_tree_is_created_written_and_committed_in_that_order() {
        let now = now();
        let mut receiver = pulling(now);

        let entry = receiver.peer_entry(1, "safe/a.txt", EntryKind::File, 3, now);
        assert!(matches!(entry.as_slice(), [Op::CreateEntry { .. }]));
        assert_eq!(
            receiver.peer_chunk(1, b"abc", now),
            vec![Op::WriteChunk {
                transfer: 1,
                bytes: b"abc".to_vec(),
            }]
        );
        assert_eq!(
            receiver.peer_complete(1, now),
            vec![Op::Commit { transfer: 1 }]
        );
    }

    #[test]
    fn a_directory_needs_no_bytes_and_a_file_gets_none_before_its_entry() {
        let now = now();
        let mut receiver = pulling(now);

        assert!(matches!(
            receiver
                .peer_entry(1, "safe", EntryKind::Directory, 0, now)
                .as_slice(),
            [Op::CreateEntry {
                kind: EntryKind::Directory,
                ..
            }]
        ));
        // Nothing is open, so a chunk here belongs to no entry at all.
        assert_eq!(
            receiver.peer_chunk(1, b"abc", now),
            cancelled(1, FileCancelReason::IoFailed)
        );
    }

    #[test]
    fn a_tree_is_not_committed_while_a_file_is_short_of_its_length() {
        let now = now();
        let mut receiver = pulling(now);

        receiver.peer_entry(1, "a.txt", EntryKind::File, 3, now);
        receiver.peer_chunk(1, b"ab", now);

        assert_eq!(
            receiver.peer_complete(1, now),
            cancelled(1, FileCancelReason::IoFailed)
        );
    }

    #[test]
    fn a_file_that_grew_past_what_it_declared_ends_the_transfer() {
        let now = now();
        let mut receiver = pulling(now);

        receiver.peer_entry(1, "a.txt", EntryKind::File, 3, now);

        assert_eq!(
            receiver.peer_chunk(1, b"abcd", now),
            cancelled(1, FileCancelReason::IoFailed)
        );
    }

    #[test]
    fn a_receiver_refuses_a_file_over_the_per_file_limit() {
        let now = now();
        let mut receiver = pulling(now);

        assert_eq!(
            receiver.peer_entry(1, "big.bin", EntryKind::File, 1025, now),
            cancelled(1, FileCancelReason::TooLarge)
        );
    }

    #[test]
    fn a_receiver_refuses_a_tree_over_the_transfer_limit() {
        let now = now();
        let mut receiver = pulling(now);

        for index in 0..4 {
            let path = format!("f{index}.bin");
            receiver.peer_entry(1, &path, EntryKind::File, 1024, now);
            receiver.peer_chunk(1, &[0u8; 1024], now);
        }

        assert_eq!(
            receiver.peer_entry(1, "one-too-many.bin", EntryKind::File, 1, now),
            cancelled(1, FileCancelReason::TooLarge)
        );
    }

    #[test]
    fn a_receiver_refuses_more_entries_than_a_tree_may_have() {
        let now = now();
        let mut receiver = pulling(now);

        for index in 0..MAX_ENTRIES {
            let path = format!("d{index}");
            assert!(matches!(
                receiver
                    .peer_entry(1, &path, EntryKind::Directory, 0, now)
                    .as_slice(),
                [Op::CreateEntry { .. }]
            ));
        }

        assert_eq!(
            receiver.peer_entry(1, "one-too-many", EntryKind::Directory, 0, now),
            cancelled(1, FileCancelReason::TooLarge)
        );
    }

    #[test]
    fn a_receiver_refuses_a_path_it_would_not_create() {
        let now = now();
        let deep = vec!["d"; 65].join("/");

        for path in ["../escape", "C:/windows", deep.as_str()] {
            let mut receiver = pulling(now);

            assert_eq!(
                receiver.peer_entry(1, path, EntryKind::Directory, 0, now),
                cancelled(1, FileCancelReason::InvalidPath),
                "{path} was not refused"
            );
        }
    }

    #[test]
    fn a_receiver_refuses_two_entries_that_are_one_file_on_windows() {
        let now = now();
        let mut receiver = pulling(now);

        receiver.peer_entry(1, "notes/a.txt", EntryKind::File, 0, now);

        assert_eq!(
            receiver.peer_entry(1, "Notes/A.TXT", EntryKind::File, 0, now),
            cancelled(1, FileCancelReason::UnsafeEntry)
        );
    }

    #[test]
    fn a_sender_enumerates_only_what_it_offered() {
        let now = now();
        let mut sender = Exchange::new(policy(), now);
        sender.local_offer(now);

        assert_eq!(
            sender.peer_request(1, 4, now),
            vec![Op::Enumerate { transfer: 4 }]
        );
        assert_eq!(
            sender.peer_request(99, 5, now),
            vec![Op::Send(Message::Cancel {
                transfer: 5,
                reason: FileCancelReason::Superseded,
            })]
        );
    }

    #[test]
    fn a_sender_holds_its_own_limits_against_what_it_was_asked_to_read() {
        let now = now();
        let mut sender = Exchange::new(policy(), now);
        sender.local_offer(now);
        sender.peer_request(1, 4, now);

        assert_eq!(
            sender.produced_entry(4, "big.bin", EntryKind::File, 1025, now),
            vec![Op::Send(Message::Cancel {
                transfer: 4,
                reason: FileCancelReason::TooLarge,
            })]
        );
    }

    #[test]
    fn a_sender_sends_one_chunk_per_call_and_completes_only_when_read_out() {
        let now = now();
        let mut sender = Exchange::new(policy(), now);
        sender.local_offer(now);
        sender.peer_request(1, 4, now);

        assert_eq!(
            sender.produced_entry(4, "a.txt", EntryKind::File, 4, now),
            vec![Op::Send(Message::Entry {
                transfer: 4,
                path: "a.txt".into(),
                kind: EntryKind::File,
                size: 4,
            })]
        );
        assert_eq!(
            sender.produced_chunk(4, b"ab".to_vec(), now),
            vec![Op::Send(Message::Chunk {
                transfer: 4,
                chunk: b"ab".to_vec(),
            })]
        );
        assert_eq!(
            sender.produced_complete(4, now),
            vec![Op::Send(Message::Cancel {
                transfer: 4,
                reason: FileCancelReason::IoFailed,
            })]
        );
    }

    #[test]
    fn a_sender_refuses_a_chunk_larger_than_a_record_may_carry() {
        let now = now();
        let mut sender = Exchange::new(policy(), now);
        sender.local_offer(now);
        sender.peer_request(1, 4, now);
        sender.produced_entry(4, "a.txt", EntryKind::File, 1024, now);

        assert_eq!(
            sender.produced_chunk(4, vec![0u8; CHUNK + 1], now),
            vec![Op::Send(Message::Cancel {
                transfer: 4,
                reason: FileCancelReason::TooLarge,
            })]
        );
    }

    #[test]
    fn the_two_directions_have_their_own_transfers() {
        let now = now();
        let mut exchange = pulling(now);
        exchange.local_offer(now);
        // The peer names its request with an id this side is already using for
        // what it is pulling.
        assert_eq!(
            exchange.peer_request(1, 1, now),
            vec![Op::Enumerate { transfer: 1 }]
        );

        // Which is the outgoing transfer, and leaves the incoming one alone.
        assert!(matches!(
            exchange
                .peer_entry(1, "a.txt", EntryKind::File, 3, now)
                .as_slice(),
            [Op::CreateEntry { .. }]
        ));
        assert_eq!(
            exchange.produced_entry(1, "b.txt", EntryKind::File, 0, now),
            vec![Op::Send(Message::Entry {
                transfer: 1,
                path: "b.txt".into(),
                kind: EntryKind::File,
                size: 0,
            })]
        );
    }

    #[test]
    fn a_new_offer_ends_the_transfer_the_old_one_was_serving() {
        let now = now();
        let mut exchange = pulling(now);
        exchange.local_offer(now);
        exchange.peer_request(1, 4, now);

        assert_eq!(
            exchange.local_offer(now),
            vec![
                Op::Send(Message::Cancel {
                    transfer: 4,
                    reason: FileCancelReason::Superseded,
                }),
                Op::Send(Message::Offer { serial: 2 })
            ]
        );
        assert_eq!(
            exchange.peer_offer(8, now),
            vec![
                Op::Send(Message::Cancel {
                    transfer: 1,
                    reason: FileCancelReason::Superseded,
                }),
                Op::Abort { transfer: 1 },
                Op::Send(Message::Request {
                    serial: 8,
                    transfer: 2,
                })
            ]
        );
    }

    #[test]
    fn a_window_that_lost_focus_carries_nothing_in_either_direction() {
        let now = now();
        let mut exchange = pulling(now);
        exchange.local_offer(now);
        exchange.peer_request(1, 4, now);

        assert_eq!(
            exchange.focus_lost(now),
            vec![
                Op::Send(Message::Cancel {
                    transfer: 4,
                    reason: FileCancelReason::FocusLost,
                }),
                Op::Send(Message::Cancel {
                    transfer: 1,
                    reason: FileCancelReason::FocusLost,
                }),
                Op::Abort { transfer: 1 },
            ]
        );
    }

    #[test]
    fn a_transfer_that_stopped_moving_is_cancelled_and_its_staging_removed() {
        let start = now();
        let mut receiver = pulling(start);
        receiver.peer_entry(1, "a.txt", EntryKind::File, 3, start);

        assert_eq!(receiver.tick(start + IDLE), Vec::new());
        assert_eq!(
            receiver.tick(start + IDLE + Duration::from_millis(1)),
            cancelled(1, FileCancelReason::TimedOut)
        );
    }

    #[test]
    fn a_record_for_a_transfer_that_is_over_is_ignored() {
        let now = now();
        let mut receiver = pulling(now);
        receiver.peer_entry(1, "a.txt", EntryKind::File, 3, now);
        receiver.peer_cancel(1, FileCancelReason::Unavailable);

        assert_eq!(receiver.peer_chunk(1, b"abc", now), Vec::new());
        assert_eq!(receiver.peer_complete(1, now), Vec::new());
        assert_eq!(
            receiver.peer_entry(1, "a.txt", EntryKind::File, 3, now),
            Vec::new()
        );
    }

    #[test]
    fn a_cancelled_incoming_transfer_takes_its_partial_tree_with_it() {
        let now = now();
        let mut receiver = pulling(now);
        receiver.peer_entry(1, "a.txt", EntryKind::File, 3, now);

        assert_eq!(
            receiver.peer_cancel(1, FileCancelReason::Unavailable),
            vec![Op::Abort { transfer: 1 }]
        );
    }

    /// What a receiver emits when it ends the transfer it is pulling.
    fn cancelled(transfer: u32, reason: FileCancelReason) -> Vec<Op> {
        vec![
            Op::Send(Message::Cancel { transfer, reason }),
            Op::Abort { transfer },
        ]
    }
}
