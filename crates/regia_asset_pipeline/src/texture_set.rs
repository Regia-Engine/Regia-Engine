// SPDX-License-Identifier: MPL-2.0
//! Texture-set detection and material generation (spec 22 T2.14, ADR-3320).
//!
//! The implementation lives in `regia_asset::texture_sets`: the generator is project-file
//! tooling that both this pipeline (through `regia assets texture-sets`) and the agent
//! protocol (`assets.texture_sets`) drive, and the agent protocol is a runtime crate that may
//! not depend on the desktop-only pipeline (`docs/06` § 7). This module re-exports it so the
//! pipeline keeps its documented home.

pub use regia_asset::texture_sets::{
    LeftoverReason, NormalGreenSetting, OrmChannelOrderSetting, TEXTURE_SUFFIX_TABLE, TextureSet,
    TextureSetConflict, TextureSetError, TextureSetLeftover, TextureSetMember, TextureSetOutcome,
    TextureSetScan, TextureSetSettings, TextureSetSlot, TextureSlotReport, TextureSuffixRow, UnreferencedMap,
    apply_channel_transforms, format_report, generate, orm_channel_order_from_file_name, scan, suffix_word,
    texture_set_slot_from_file_name, usage_from_file_name,
};
