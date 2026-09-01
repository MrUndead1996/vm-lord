//! Proving that the peer on the other end of a display session holds the VM's
//! secret, and keying the channels that hang off that proof.
//!
//! The root of trust is the per-VM secret the agent protocol already
//! mints -- 32 bytes written into the seed as `/etc/vmlord/agent.secret`,
//! root-only. It never travels on this protocol and it never reaches the
//! unprivileged capture process: the privileged broker in the guest derives a
//! session key from it and hands only that on. Compromising the session
//! process costs one session, not the VM's identity.
//!
//! Both ends compute everything here with the same functions, which is what
//! keeps them from disagreeing about what is being signed.

use std::{error::Error, fmt};

use base64::{Engine, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::record::Channel;

/// The width of a secret, a session key and a channel key.
pub const SECRET_LEN: usize = 32;

/// The width of each side's handshake nonce.
pub const NONCE_LEN: usize = 32;

/// The width of the identifier that names a session across its five sockets.
pub const SESSION_ID_LEN: usize = 16;

/// The width of a tag, which is HMAC-SHA-256's output.
pub const TAG_LEN: usize = 32;

/// Separates this protocol's session keys from every other use of the secret.
const SESSION_DOMAIN: &[u8] = b"vmlord.display.v1.session";

/// Separates the transcript hash from any other SHA-256 in the system.
const TRANSCRIPT_DOMAIN: &[u8] = b"vmlord.display.v1.transcript";

/// Separates a channel key from the session key it comes from.
const CHANNEL_DOMAIN: &[u8] = b"vmlord.display.v1.channel";

/// Which end of a session a tag speaks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// VMLord, which connects.
    Host,
    /// The guest's display services, which listen.
    Guest,
}

impl Role {
    /// What this role's tags are labelled with.
    ///
    /// The two labels are what keep a tag one side produced from being
    /// replayed back at it as the other side's proof.
    #[must_use]
    pub fn label(self) -> &'static [u8] {
        match self {
            Self::Host => b"client",
            Self::Guest => b"server",
        }
    }
}

/// A VM's shared secret.
///
/// No `Debug` and no `Display`, by design: the one thing this type must never
/// do is print itself.
pub struct Secret(Zeroizing<[u8; SECRET_LEN]>);

impl Secret {
    /// Mints a secret from the operating system's random source.
    #[must_use]
    pub fn generate() -> Self {
        Self(Zeroizing::new(random_bytes()))
    }

    /// Reads a secret back from the form it is stored and delivered in.
    ///
    /// Surrounding whitespace is ignored, because the file this comes out of
    /// is a text file whose ends may have been written by something that ends
    /// lines.
    ///
    /// # Errors
    ///
    /// [`SecretError`] if the text is not base64 or does not decode to exactly
    /// [`SECRET_LEN`] bytes. Both mean the secret is truncated or is not one,
    /// and neither is recovered from by padding.
    pub fn from_base64(text: &str) -> Result<Self, SecretError> {
        let bytes = Zeroizing::new(STANDARD.decode(text.trim()).map_err(|_| SecretError)?);
        let bytes: [u8; SECRET_LEN] = bytes.as_slice().try_into().map_err(|_| SecretError)?;

        Ok(Self(Zeroizing::new(bytes)))
    }

    /// A second handle on the same secret, for a type that has to own one.
    ///
    /// Not `Clone`: copying a secret is a thing to do deliberately and rarely,
    /// and a derive would make it look ordinary.
    pub(crate) fn duplicate(&self) -> Self {
        Self(Zeroizing::new(*self.0))
    }

    /// The secret as base64, which is how it is written to a file.
    #[must_use]
    pub fn to_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.0.as_slice()))
    }
}

/// The key one display session's proofs and channel keys are built on.
pub struct SessionKey(Zeroizing<[u8; SECRET_LEN]>);

impl SessionKey {
    #[cfg(test)]
    fn expose(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }
}

/// The key one channel of one session proves itself with.
pub struct ChannelKey(Zeroizing<[u8; SECRET_LEN]>);

impl ChannelKey {
    #[cfg(test)]
    fn expose(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    /// The key's bytes, for the one caller that must hand it to another
    /// process.
    ///
    /// [`SessionKey`] has no such method and must not grow one: a session key
    /// opens every channel and outlives them, while this one opens a single
    /// channel of a single session and is worthless the moment that session
    /// ends. That is the whole reason the broker derives channel keys and
    /// passes those on rather than the secret.
    ///
    /// The bytes come back in a [`Zeroizing`] wrapper, so a copy that is only
    /// serialised and dropped does not stay in this process's memory.
    #[must_use]
    pub fn to_bytes(&self) -> Zeroizing<[u8; SECRET_LEN]> {
        Zeroizing::new(*self.0)
    }

    /// A key from the bytes [`ChannelKey::to_bytes`] produced.
    ///
    /// The other half of handing a channel key to another process. It derives
    /// nothing and checks nothing: a key is whatever the process that derived
    /// it says, and a wrong one simply fails to bind.
    #[must_use]
    pub fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }
}

/// Derives the session key both peers authenticate with.
///
/// The nonces are the salt and the session id is in the info, so a key is good
/// for one session and no other; a recorded tag is worthless the moment the
/// next session draws its nonces.
#[must_use]
pub fn session_key(
    secret: &Secret,
    session_id: &[u8; SESSION_ID_LEN],
    host_nonce: &[u8; NONCE_LEN],
    guest_nonce: &[u8; NONCE_LEN],
) -> SessionKey {
    let mut salt = [0u8; NONCE_LEN * 2];
    salt[..NONCE_LEN].copy_from_slice(host_nonce);
    salt[NONCE_LEN..].copy_from_slice(guest_nonce);

    let mut info = Vec::with_capacity(SESSION_DOMAIN.len() + SESSION_ID_LEN);
    info.extend_from_slice(SESSION_DOMAIN);
    info.extend_from_slice(session_id);

    let mut key = Zeroizing::new([0u8; SECRET_LEN]);
    Hkdf::<Sha256>::new(Some(&salt), secret.0.as_slice())
        .expand(&info, key.as_mut_slice())
        .expect("32 bytes is far below HKDF-SHA-256's output limit");

    SessionKey(key)
}

/// The running hash of the handshake, over the bytes as they crossed the wire.
///
/// Protobuf does not promise that the same message encodes to the same bytes
/// twice, so a transcript over a re-encoded message is one two correct
/// implementations can disagree about. Every payload is length-prefixed into
/// the hash, so that two records cannot slide into one.
pub struct Transcript(Sha256);

impl Transcript {
    /// Starts a transcript, domain-separated from every other SHA-256.
    #[must_use]
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TRANSCRIPT_DOMAIN);
        Self(hasher)
    }

    /// Adds one handshake payload, exactly as it appeared on the wire.
    pub fn record(&mut self, payload: &[u8]) {
        self.0.update(
            u32::try_from(payload.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        self.0.update(payload);
    }

    /// The hash of everything recorded so far.
    #[must_use]
    pub fn finish(&self) -> [u8; 32] {
        self.0.clone().finalize().into()
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

/// Derives the key a frame or input channel proves itself with.
///
/// It depends on the transcript, which is why a socket cannot be carried in
/// from another session or offered by a process that did not take part in the
/// control handshake.
#[must_use]
pub fn channel_key(session: &SessionKey, transcript: &[u8; 32], channel: Channel) -> ChannelKey {
    let mut info = Vec::with_capacity(CHANNEL_DOMAIN.len() + 32 + 1);
    info.extend_from_slice(CHANNEL_DOMAIN);
    info.extend_from_slice(transcript);
    info.push(channel.as_wire());

    let mut key = Zeroizing::new([0u8; SECRET_LEN]);
    Hkdf::<Sha256>::from_prk(session.0.as_slice())
        .expect("a 32-byte pseudo-random key is long enough for SHA-256")
        .expand(&info, key.as_mut_slice())
        .expect("32 bytes is far below HKDF-SHA-256's output limit");

    ChannelKey(key)
}

/// The proof that a peer holds the key a tag was computed under.
///
/// A tag says nothing about the key and is worthless once its session is over,
/// so unlike a [`Secret`] it may be copied and printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag([u8; TAG_LEN]);

impl Tag {
    /// The bytes to put in a `ServerAuth`, `ClientAuth`, `ChannelAck` or
    /// `ChannelAuth`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; TAG_LEN] {
        &self.0
    }

    /// Reads a tag out of a message that arrived on the wire.
    ///
    /// # Errors
    ///
    /// [`WrongLength`] for anything other than [`TAG_LEN`] bytes.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WrongLength> {
        Ok(Self(bytes.try_into().map_err(|_| WrongLength {
            what: "tag",
            len: bytes.len(),
        })?))
    }
}

/// The tag `role` puts on the control handshake's transcript.
#[must_use]
pub fn control_tag(session: &SessionKey, role: Role, transcript: &[u8; 32]) -> Tag {
    let mut mac = Hmac::<Sha256>::new_from_slice(session.0.as_slice())
        .expect("HMAC accepts a key of any length");
    mac.update(role.label());
    mac.update(transcript);

    Tag(mac.finalize().into_bytes().into())
}

/// The tag `role` puts on a frame or input channel's exchange.
#[must_use]
pub fn channel_tag(
    key: &ChannelKey,
    role: Role,
    channel: Channel,
    host_nonce: &[u8; NONCE_LEN],
    guest_nonce: &[u8; NONCE_LEN],
) -> Tag {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.0.as_slice()).expect("HMAC accepts a key of any length");
    mac.update(role.label());
    mac.update(&[channel.as_wire()]);
    mac.update(host_nonce);
    mac.update(guest_nonce);

    Tag(mac.finalize().into_bytes().into())
}

/// Compares two tags without leaking where they differ.
///
/// An early return on the first differing byte is how a tag gets forged a byte
/// at a time.
#[must_use]
pub fn verify(expected: &Tag, offered: &Tag) -> bool {
    expected.0.ct_eq(&offered.0).into()
}

/// Draws bytes from the operating system's random source.
///
/// # Panics
///
/// If the platform's random source fails. There is no session to open without
/// a fresh nonce, and continuing with a predictable one is worse than
/// stopping.
#[must_use]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the operating system has a random source");
    bytes
}

/// Text that is not a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretError;

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a display secret must be {SECRET_LEN} base64-encoded bytes"
        )
    }
}

impl Error for SecretError {}

/// A fixed-width field that arrived at another width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrongLength {
    /// Which field: `"tag"`, `"nonce"` or `"session id"`.
    pub what: &'static str,
    /// What arrived.
    pub len: usize,
}

impl fmt::Display for WrongLength {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a {} of {} bytes is the wrong width",
            self.what, self.len
        )
    }
}

impl Error for WrongLength {}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> Secret {
        Secret::from_base64(&Secret::generate().to_base64()).expect("what generate produced")
    }

    fn transcript_of(payloads: &[&[u8]]) -> [u8; 32] {
        let mut transcript = Transcript::new();
        for payload in payloads {
            transcript.record(payload);
        }
        transcript.finish()
    }

    #[test]
    fn a_secret_survives_the_form_it_is_delivered_in() {
        let secret = Secret::generate();
        let text = secret.to_base64();

        let read_back = Secret::from_base64(&format!("  {}\n", *text)).expect("surrounding space");

        assert_eq!(
            session_key(&secret, &[1; 16], &[2; 32], &[3; 32]).expose(),
            session_key(&read_back, &[1; 16], &[2; 32], &[3; 32]).expose()
        );
    }

    #[test]
    fn a_secret_that_is_not_thirty_two_bytes_is_refused() {
        assert!(Secret::from_base64("c2hvcnQ=").is_err());
        assert!(Secret::from_base64("not base64!").is_err());
    }

    #[test]
    fn a_session_key_changes_with_every_input() {
        let secret = secret();
        let base = session_key(&secret, &[1; 16], &[2; 32], &[3; 32]);

        assert_ne!(
            base.expose(),
            session_key(&secret, &[9; 16], &[2; 32], &[3; 32]).expose()
        );
        assert_ne!(
            base.expose(),
            session_key(&secret, &[1; 16], &[9; 32], &[3; 32]).expose()
        );
        assert_ne!(
            base.expose(),
            session_key(&secret, &[1; 16], &[2; 32], &[9; 32]).expose()
        );
        assert_ne!(
            base.expose(),
            session_key(&Secret::generate(), &[1; 16], &[2; 32], &[3; 32]).expose()
        );
    }

    #[test]
    fn a_transcript_depends_on_the_order_and_the_boundaries_of_what_it_recorded() {
        assert_ne!(
            transcript_of(&[b"client", b"server"]),
            transcript_of(&[b"server", b"client"])
        );
        // The length prefix is what keeps two records from sliding into one.
        assert_ne!(transcript_of(&[b"ab", b"c"]), transcript_of(&[b"a", b"bc"]));
    }

    #[test]
    fn the_two_roles_sign_the_same_transcript_differently() {
        let key = session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]);
        let transcript = transcript_of(&[b"client hello", b"server hello"]);

        assert_ne!(
            control_tag(&key, Role::Host, &transcript).as_bytes(),
            control_tag(&key, Role::Guest, &transcript).as_bytes()
        );
    }

    #[test]
    fn a_tag_is_only_good_for_the_transcript_it_was_made_over() {
        let key = session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]);
        let mine = control_tag(&key, Role::Guest, &transcript_of(&[b"hello"]));
        let theirs = control_tag(&key, Role::Guest, &transcript_of(&[b"hell0"]));

        assert!(verify(&mine, &mine));
        assert!(!verify(&mine, &theirs));
    }

    #[test]
    fn a_channel_key_is_bound_to_the_transcript_and_the_channel() {
        let key = session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]);
        let transcript = transcript_of(&[b"client hello", b"server hello"]);

        let frame = channel_key(&key, &transcript, Channel::Frame);
        let input = channel_key(&key, &transcript, Channel::Input);
        let other_session = channel_key(&key, &transcript_of(&[b"elsewhere"]), Channel::Frame);

        assert_ne!(frame.expose(), input.expose());
        assert_ne!(frame.expose(), other_session.expose());
    }

    #[test]
    fn a_channel_tag_covers_both_nonces() {
        let key = channel_key(
            &session_key(&secret(), &[1; 16], &[2; 32], &[3; 32]),
            &transcript_of(&[b"hello"]),
            Channel::Frame,
        );

        let tag = channel_tag(&key, Role::Guest, Channel::Frame, &[4; 32], &[5; 32]);

        assert!(!verify(
            &tag,
            &channel_tag(&key, Role::Guest, Channel::Frame, &[4; 32], &[6; 32])
        ));
        assert!(!verify(
            &tag,
            &channel_tag(&key, Role::Guest, Channel::Input, &[4; 32], &[5; 32])
        ));
    }

    #[test]
    fn a_tag_of_the_wrong_length_is_refused_rather_than_padded() {
        assert!(Tag::from_wire(&[0u8; 31]).is_err());
        assert!(Tag::from_wire(&[0u8; 32]).is_ok());
    }
}
