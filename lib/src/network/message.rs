use super::{
    crypto::Role,
    debug_payload::{DebugRequest, DebugResponse},
    peer_exchange::PexPayload,
    runtime_id::PublicRuntimeId,
};
use crate::{
    crypto::{sign::PublicKey, Hash, Hashable},
    protocol::{
        BlockContent, BlockId, BlockNonce, InnerNodes, LeafNodes, MultiBlockPresence,
        UntrustedProof,
    },
    repository::RepositoryId,
};
use serde::{Deserialize, Serialize};
use std::{fmt, io::Write};

#[derive(Clone, PartialEq, Serialize, Deserialize, Debug)]
pub(crate) enum Request {
    /// Request the latest root node of the given writer.
    RootNode(PublicKey, DebugRequest),
    /// Request child nodes of the given parent node.
    ChildNodes(Hash, ResponseDisambiguator, DebugRequest),
    /// Request block with the given id.
    Block(BlockId, DebugRequest),
}

/// ResponseDisambiguator is used to uniquelly assign a response to a request.
/// What we want to avoid is that an outdated response clears out a newer pending request.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
#[serde(transparent)]
pub(crate) struct ResponseDisambiguator(MultiBlockPresence);

impl ResponseDisambiguator {
    pub fn new(multi_block_presence: MultiBlockPresence) -> Self {
        Self(multi_block_presence)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Response {
    /// Send the latest root node of this replica to another replica.
    /// NOTE: This is both a response and notification - the server sends this as a response to
    /// `Request::RootNode` but also on its own when it detects change in the repo.
    RootNode(UntrustedProof, MultiBlockPresence, DebugResponse),
    /// Send that a RootNode request failed
    RootNodeError(PublicKey, DebugResponse),
    /// Send inner nodes.
    InnerNodes(InnerNodes, ResponseDisambiguator, DebugResponse),
    /// Send leaf nodes.
    LeafNodes(LeafNodes, ResponseDisambiguator, DebugResponse),
    /// Send that a ChildNodes request failed
    ChildNodesError(Hash, ResponseDisambiguator, DebugResponse),
    /// Send a notification that a block became available on this replica.
    /// NOTE: This is always unsolicited - the server sends it on its own when it detects a newly
    /// received block.
    BlockOffer(BlockId, DebugResponse),
    /// Send a requested block.
    Block(BlockContent, BlockNonce, DebugResponse),
    /// Send that a Block request failed
    BlockError(BlockId, DebugResponse),
}

const LEGACY_TAG: u8 = 2;

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize, Debug)]
pub(crate) struct Header {
    pub channel: MessageChannelId,
}

impl Header {
    pub(crate) const SIZE: usize = 1 + // One byte for the tag.
        Hash::SIZE; // Channel

    pub(crate) fn serialize(&self) -> [u8; Self::SIZE] {
        let mut hdr = [0; Self::SIZE];
        let mut w = ArrayWriter { array: &mut hdr };

        w.write_u8(LEGACY_TAG);
        w.write_channel(&self.channel);

        hdr
    }

    pub(crate) fn deserialize(hdr: &[u8; Self::SIZE]) -> Option<Header> {
        let mut r = ArrayReader { array: &hdr[..] };
        // Tag is no longer used but we still read it for backwards compatibility.
        let _ = r.read_u8();
        let channel = r.read_channel();

        Some(Header { channel })
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Message {
    pub channel: MessageChannelId,
    pub content: Vec<u8>,
}

impl Message {
    pub fn header(&self) -> Header {
        Header {
            channel: self.channel,
        }
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Message {{ channel: {:?}, content-hash: {:?} }}",
            self.channel,
            self.content.hash()
        )
    }
}

struct ArrayReader<'a> {
    array: &'a [u8],
}

impl ArrayReader<'_> {
    // Unwraps are OK because all sizes are known at compile time.

    fn read_u8(&mut self) -> u8 {
        let n = u8::from_le_bytes(self.array[..1].try_into().unwrap());
        self.array = &self.array[1..];
        n
    }

    fn read_channel(&mut self) -> MessageChannelId {
        let hash: [u8; Hash::SIZE] = self.array[..Hash::SIZE].try_into().unwrap();
        self.array = &self.array[Hash::SIZE..];
        hash.into()
    }
}

struct ArrayWriter<'a> {
    array: &'a mut [u8],
}

impl ArrayWriter<'_> {
    // Unwraps are OK because all sizes are known at compile time.

    fn write_u8(&mut self, n: u8) {
        self.array.write_all(&n.to_le_bytes()).unwrap();
    }

    fn write_channel(&mut self, channel: &MessageChannelId) {
        self.array.write_all(channel.as_ref()).unwrap();
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Content {
    Request(Request),
    Response(Response),
    // Peer exchange
    Pex(PexPayload),
}

#[cfg(test)]
impl From<Content> for Request {
    fn from(content: Content) -> Self {
        match content {
            Content::Request(request) => request,
            Content::Response(_) | Content::Pex(_) => {
                panic!("not a request: {:?}", content)
            }
        }
    }
}

#[cfg(test)]
impl From<Content> for Response {
    fn from(content: Content) -> Self {
        match content {
            Content::Response(response) => response,
            Content::Request(_) | Content::Pex(_) => {
                panic!("not a response: {:?}", content)
            }
        }
    }
}

define_byte_array_wrapper! {
    // TODO: consider lower size (truncate the hash) which should still be enough to be unique
    // while reducing the message size.
    #[derive(Serialize, Deserialize)]
    pub(crate) struct MessageChannelId([u8; Hash::SIZE]);
}

impl MessageChannelId {
    pub(super) fn new(
        repo_id: &'_ RepositoryId,
        this_runtime_id: &'_ PublicRuntimeId,
        that_runtime_id: &'_ PublicRuntimeId,
        role: Role,
    ) -> Self {
        let (id1, id2) = match role {
            Role::Initiator => (this_runtime_id, that_runtime_id),
            Role::Responder => (that_runtime_id, this_runtime_id),
        };

        Self(
            (repo_id, id1, id2, b"ouisync message channel id")
                .hash()
                .into(),
        )
    }

    #[cfg(test)]
    pub(crate) fn random() -> Self {
        Self(rand::random())
    }
}

impl Default for MessageChannelId {
    fn default() -> Self {
        Self([0; Self::SIZE])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_serialization() {
        let header = Header {
            channel: MessageChannelId::random(),
        };

        let serialized = header.serialize();
        assert_eq!(Header::deserialize(&serialized), Some(header));
    }
}
