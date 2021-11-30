//! Encryption protocol for syncing repositories.
//!
//! Using the "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s" protocol from
//! [Noise Protocol Framework](https://noiseprotocol.org/noise.html).
//!
//! Using the salted hash of the secret repository id as the pre-shared key. This way only the
//! replicas that posses the secret repository id are able to communicate and no authentication
//! based on the identity of the replicas is needed.

use super::{
    message_dispatcher::{ContentSink, ContentStream},
    runtime_id::RuntimeId,
};
use crate::repository::{PublicRepositoryId, SecretRepositoryId};
use noise_protocol::Cipher as _;
use noise_rust_crypto::{Blake2s, ChaCha20Poly1305, X25519};
use std::mem;
use thiserror::Error;

type Cipher = ChaCha20Poly1305;
type CipherState = noise_protocol::CipherState<Cipher>;
type HandshakeState = noise_protocol::HandshakeState<X25519, Cipher, Blake2s>;

/// Role of this replica in the communication protocol.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Role {
    /// Initiator sends the first message
    Initiator,
    /// Responder waits for the first message and then responds
    Responder,
}

impl Role {
    /// Determine the role this replica will have in the communication protocol for the given
    /// repository.
    ///
    /// # Panics
    ///
    /// Panics if the runtime ids are equal.
    pub fn determine(
        repo_id: &SecretRepositoryId,
        this_runtime_id: &RuntimeId,
        that_runtime_id: &RuntimeId,
    ) -> Self {
        assert_ne!(this_runtime_id, that_runtime_id);

        let this_hash = repo_id.salted_hash(this_runtime_id.as_ref());
        let that_hash = repo_id.salted_hash(that_runtime_id.as_ref());

        if this_hash > that_hash {
            Role::Initiator
        } else {
            Role::Responder
        }
    }
}

// This also determines the maximum number of messages we can send or receive in a single protocol
// session.
const MAX_NONCE: u64 = u64::MAX - 1;

/// Wrapper for [`ContentStream`] that decrypts incoming messages.
pub(super) struct DecryptingStream {
    inner: ContentStream,
    cipher: CipherState,
    buffer: Vec<u8>,
}

impl DecryptingStream {
    pub async fn recv(&mut self) -> Result<Vec<u8>, Error> {
        if self.cipher.get_next_n() >= MAX_NONCE {
            return Err(Error::Exhausted);
        }

        let mut content = self.inner.recv().await.ok_or(Error::Closed)?;

        let plain_len = content
            .len()
            .checked_sub(Cipher::tag_len())
            .ok_or(Error::Crypto)?;
        self.buffer.resize(plain_len, 0);
        self.cipher
            .decrypt(&content, &mut self.buffer)
            .map_err(|_| Error::Crypto)?;

        mem::swap(&mut content, &mut self.buffer);

        Ok(content)
    }

    pub fn id(&self) -> &PublicRepositoryId {
        self.inner.id()
    }
}

/// Wrapper for [`ContentSink`] that encrypts outgoing messages.
pub(super) struct EncryptingSink {
    inner: ContentSink,
    cipher: CipherState,
    buffer: Vec<u8>,
}

impl EncryptingSink {
    pub async fn send(&mut self, mut content: Vec<u8>) -> Result<(), Error> {
        if self.cipher.get_next_n() >= MAX_NONCE {
            return Err(Error::Exhausted);
        }

        self.buffer.resize(content.len() + Cipher::tag_len(), 0);
        self.cipher.encrypt(&content, &mut self.buffer);

        mem::swap(&mut content, &mut self.buffer);

        if self.inner.send(content).await {
            Ok(())
        } else {
            Err(Error::Closed)
        }
    }

    pub fn id(&self) -> &PublicRepositoryId {
        self.inner.id()
    }
}

/// Establish encrypted communication channel for the purpose of syncing the given
/// repository.
pub(super) async fn establish_channel(
    role: Role,
    repo_id: &SecretRepositoryId,
    mut stream: ContentStream,
    sink: ContentSink,
) -> Result<(DecryptingStream, EncryptingSink), Error> {
    let mut handshake_state = build_handshake_state(role, repo_id);

    let (recv_cipher, send_cipher) = match role {
        Role::Initiator => {
            let content = handshake_state.write_message_vec(&[])?;
            if !sink.send(content).await {
                return Err(Error::Closed);
            }

            let content = stream.recv().await.ok_or(Error::Closed)?;
            handshake_state.read_message_vec(&content)?;

            assert!(handshake_state.completed());

            let (send_cipher, recv_cipher) = handshake_state.get_ciphers();
            (recv_cipher, send_cipher)
        }
        Role::Responder => {
            let content = stream.recv().await.ok_or(Error::Closed)?;
            handshake_state.read_message_vec(&content)?;

            let content = handshake_state.write_message_vec(&[])?;
            if !sink.send(content).await {
                return Err(Error::Closed);
            }

            assert!(handshake_state.completed());

            handshake_state.get_ciphers()
        }
    };

    let stream = DecryptingStream {
        inner: stream,
        cipher: recv_cipher,
        buffer: vec![],
    };

    let sink = EncryptingSink {
        inner: sink,
        cipher: send_cipher,
        buffer: vec![],
    };

    Ok((stream, sink))
}

#[derive(Debug, Error)]
pub(super) enum Error {
    #[error("encryption / decryption failed")]
    Crypto,
    #[error("send / receive failed - channel closed")]
    Closed,
    #[error("nonce counter exhausted")]
    Exhausted,
}

impl From<noise_protocol::Error> for Error {
    fn from(_: noise_protocol::Error) -> Self {
        Self::Crypto
    }
}

fn build_handshake_state(role: Role, repo_id: &SecretRepositoryId) -> HandshakeState {
    use noise_protocol::patterns;

    let mut state = HandshakeState::new(
        patterns::noise_nn_psk0(),
        role == Role::Initiator,
        &[],
        None,
        None,
        None,
        None,
    );
    state.push_psk(repo_id.salted_hash(b"pre-shared-key").as_ref());
    state
}
