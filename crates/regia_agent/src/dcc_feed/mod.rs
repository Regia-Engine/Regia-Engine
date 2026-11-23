// SPDX-License-Identifier: MPL-2.0
//! The DCC scene feed (`specs/60`): a development-time channel that lets a content tool stream
//! scene data into a running editor or dev game.
//!
//! The pure, socket-free parts are testable without a DCC, a GPU or a network:
//!
//! - [`wire`]: the binary protocol v1 codec (`docs/formats/dcc-feed-protocol-v1.md`),
//! - [`recording`]: the `.rfdrec` v1 recording format (`docs/formats/rfdrec.md`),
//! - [`inbox`]: the bounded inbox with coalescing and byte credits (spec § 4.7),
//! - [`playout`]: snap and delayed playout buffers (spec § 2.6),
//! - [`session`]: the session state machine and the hello checks (spec § 2.1, § 9.1),
//! - [`registry`]: items, handles and the ownership rules between sessions (spec § 2.2 to § 2.4),
//! - [`core`]: what each decoded message does, replies, recording and replay (spec § 3.4),
//! - [`settings`]: `Regia.toml [dcc_feed]` (spec § 3.2),
//! - [`mesh`]: mesh items and generated normals (spec § 4.5),
//! - [`item_guid`]: stable item ids from keys (spec § 2.3).
//!
//! [`net`] and [`ws`] open the sockets: reader threads decode and validate. [`plugin`] holds the ECS side:
//! [`plugin::DccFeedPlugin`], its resources and messages; [`methods`] the `dcc_feed.*` agent methods.

mod apply;
pub mod core;
pub mod inbox;
pub mod mesh;
pub mod methods;
pub mod net;
pub mod playout;
pub mod plugin;
pub mod recording;
pub mod registry;
pub mod session;
pub mod settings;
pub mod wire;
pub mod ws;

// Every test here uses mesh items, so the module needs the asset kinds.
#[cfg(all(test, feature = "dcc-feed-assets"))]
mod tests;

pub use plugin::{
    DccFeedClock, DccFeedPlugin, DccFeedRegistry, DccFeedServer, DccFeedSet, DccFeedWarning, FeedChannels,
    FeedClip, FeedItem, FeedItemChange, FeedItemChanged, FeedMeshPart, FeedSessionChanged, FeedSkeleton,
    FeedTexture,
};
pub use settings::DccFeedSettings;

use regia_core::id::Guid;

/// Fixed namespace of item ids, hashed ahead of the key.
const ITEM_NAMESPACE: &[u8; 16] = b"regia-dcc-feed-1";

/// The stable id of the item with `key`: a name-based 128-bit id from a fixed namespace and the
/// key (spec § 2.3). The same key gives the same id in every session and on every machine.
///
/// The construction is RFC 9562's name-based layout (version and variant bits set) with BLAKE3
/// as the hash instead of SHA-1, so it adds no dependency (ADR-1669).
#[must_use]
pub fn item_guid(key: &str) -> Guid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ITEM_NAMESPACE);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80; // version 8: custom, name-based
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC variant
    Guid::from_u128(u128::from_be_bytes(bytes))
}

#[cfg(test)]
mod guid_tests {
    use super::*;

    #[test]
    fn guid_is_stable_and_key_dependent() {
        assert_eq!(item_guid("Cube"), item_guid("Cube"));
        assert_ne!(item_guid("Cube"), item_guid("Cube.001"));
        let bytes = item_guid("Cube").0.to_be_bytes();
        assert_eq!(bytes[6] >> 4, 8);
        assert_eq!(bytes[8] >> 6, 2);
    }

    /// Pinned vector: a change here changes every feed-owned entity id and every `FeedAttach`
    /// resolution, so it needs a deliberate decision.
    #[test]
    fn guid_vector() {
        assert_eq!(item_guid("Cube").0, ITEM_CUBE_VECTOR);
    }

    const ITEM_CUBE_VECTOR: u128 = 0xbb678243830b81ab965f37b74936f909;
}
