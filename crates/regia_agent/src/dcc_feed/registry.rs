// SPDX-License-Identifier: MPL-2.0
//! The item table (`specs/60` § 2.2 to § 2.4): keys, stable ids, handle mapping, item states and
//! the ownership rules between sessions.
//!
//! Pure data, ordered by id so iteration (and so entity creation and the agent listings) is
//! deterministic. The ECS side (`apply`) keeps the entity of each item here.

use std::collections::{BTreeMap, HashMap};

use regia_core::id::Guid;
use regia_ecs::prelude::*;

use super::item_guid;
use super::session::SessionId;
use super::wire::{ErrorCode, FeedKind, FrameInfo, ItemDeclare, Limits, Timecode, WireError};

/// Item states (spec § 2.2). `baked` and `removed` are terminal: such items leave the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
pub enum ItemState {
    /// Declared, no frame applied yet.
    Declared,
    /// Frames arrive within `stall_after_ms`.
    Active,
    /// Connected, silent for `stall_after_ms`.
    Stalled,
    /// The session closed; the entity stays, frozen, still feed-owned.
    Detached,
    /// Baked into ordinary content (terminal).
    Baked,
    /// Deleted by the sender or purged (terminal).
    Removed,
}

impl ItemState {
    /// The lowercase name used by `dcc_feed.items` and `FeedItem`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Active => "active",
            Self::Stalled => "stalled",
            Self::Detached => "detached",
            Self::Baked => "baked",
            Self::Removed => "removed",
        }
    }

    /// Parses a name (the `state` filter of `dcc_feed.items`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "declared" => Self::Declared,
            "active" => Self::Active,
            "stalled" => Self::Stalled,
            "detached" => Self::Detached,
            "baked" => Self::Baked,
            "removed" => Self::Removed,
            _ => return None,
        })
    }
}

/// Size details of an item (`dcc_feed.items` `detail`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemDetail {
    /// Vertices of the current mesh.
    pub vertices: u32,
    /// Triangles of the current mesh.
    pub triangles: u32,
    /// Joints of a skeleton item.
    pub joints: u32,
    /// Channels of a generic item.
    pub channels: u32,
}

/// One item.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemRecord {
    /// The sender's stable key.
    pub key: String,
    /// The stable id from the key.
    pub guid: Guid,
    /// What the item drives.
    pub kind: FeedKind,
    /// Display name from the declaration.
    pub display_name: String,
    /// Light or camera variant byte.
    pub variant: u8,
    /// Channel names of a generic item.
    pub channels: Vec<String>,
    /// Parent item, when declared under one.
    pub parent: Option<Guid>,
    /// Owning session (`None` once detached).
    pub session: Option<SessionId>,
    /// The session-local handle (0 once detached).
    pub handle: u32,
    /// Current state.
    pub state: ItemState,
    /// The entity, once the apply step spawned it.
    pub entity: Option<Entity>,
    /// Frames applied.
    pub frames_applied: u64,
    /// Last applied `frame_id`.
    pub last_frame_id: u64,
    /// Last applied `source_time_ns`.
    pub last_source_time_ns: u64,
    /// Engine time of the last applied frame (or of the declaration).
    pub last_frame_s: f64,
    /// Timecode of the last applied frame.
    pub timecode: Option<Timecode>,
    /// Frames dropped for a non-increasing `frame_id`.
    pub dropped_out_of_order: u64,
    /// Size details.
    pub detail: ItemDetail,
}

/// What a declaration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declared {
    /// The item's id.
    pub guid: Guid,
    /// True for a new item, false when an existing (detached or same-session) item was rebound.
    pub fresh: bool,
}

/// Every item of one world.
#[derive(Debug, Default)]
pub struct ItemTable {
    items: BTreeMap<Guid, ItemRecord>,
    handles: HashMap<(SessionId, u32), Guid>,
}

impl ItemTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The item `guid`.
    #[must_use]
    pub fn get(&self, guid: Guid) -> Option<&ItemRecord> {
        self.items.get(&guid)
    }

    /// The item `guid`, mutable.
    pub fn get_mut(&mut self, guid: Guid) -> Option<&mut ItemRecord> {
        self.items.get_mut(&guid)
    }

    /// Every item, ordered by id.
    pub fn iter(&self) -> impl Iterator<Item = &ItemRecord> {
        self.items.values()
    }

    /// Number of items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Items owned by `session`.
    #[must_use]
    pub fn session_items(&self, session: SessionId) -> usize {
        self.items.values().filter(|i| i.session == Some(session)).count()
    }

    /// The item a session handle addresses.
    ///
    /// # Errors
    ///
    /// `unknown_item` (not fatal) for an undeclared handle.
    pub fn resolve(&self, session: SessionId, handle: u32) -> Result<Guid, WireError> {
        self.handles
            .get(&(session, handle))
            .copied()
            .ok_or_else(|| WireError::new(ErrorCode::UnknownItem, format!("handle {handle} is not declared")))
    }

    /// Applies a declaration (spec § 2.4 rules 4 to 6).
    ///
    /// # Errors
    ///
    /// `limit_exceeded` (key length, items per session), `item_conflict` (another session owns
    /// the key), `kind_mismatch` (the key exists with another kind). None is fatal.
    pub fn declare(
        &mut self,
        session: SessionId,
        d: &ItemDeclare,
        limits: &Limits,
        now_s: f64,
    ) -> Result<Declared, WireError> {
        if d.key.len() > limits.max_key_len as usize {
            return Err(WireError::new(
                ErrorCode::LimitExceeded,
                "key longer than max_key_len",
            ));
        }
        let guid = item_guid(&d.key);
        let parent = if d.parent_handle == 0 {
            None
        } else {
            self.handles.get(&(session, d.parent_handle)).copied()
        };
        if let Some(existing) = self.items.get_mut(&guid) {
            if let Some(owner) = existing.session
                && owner != session
            {
                return Err(WireError::new(
                    ErrorCode::ItemConflict,
                    format!("key {:?} is owned by session {owner}", d.key),
                ));
            }
            if existing.kind != d.kind {
                return Err(WireError::new(
                    ErrorCode::KindMismatch,
                    format!(
                        "key {:?} is a {} item; delete it first",
                        d.key,
                        existing.kind.name()
                    ),
                ));
            }
            let old_handle = (existing.session.unwrap_or(session), existing.handle);
            let rebound = existing.session.is_none();
            existing.session = Some(session);
            existing.handle = d.handle;
            existing.display_name.clone_from(&d.display_name);
            existing.variant = d.variant;
            existing.channels.clone_from(&d.channels);
            existing.parent = parent;
            if rebound {
                // Detached -> active on rebinding (spec § 2.2).
                existing.state = ItemState::Active;
                existing.last_frame_s = now_s;
                // A new session numbers frames from its own start.
                existing.frames_applied = 0;
                existing.last_frame_id = 0;
            }
            if self.handles.get(&old_handle) == Some(&guid) {
                self.handles.remove(&old_handle);
            }
            self.handles.insert((session, d.handle), guid);
            return Ok(Declared { guid, fresh: false });
        }
        if self.session_items(session) >= limits.max_items as usize {
            return Err(WireError::new(
                ErrorCode::LimitExceeded,
                "max_items reached for this session",
            ));
        }
        // A handle reused for another key replaces the old mapping.
        self.handles.insert((session, d.handle), guid);
        self.items.insert(
            guid,
            ItemRecord {
                key: d.key.clone(),
                guid,
                kind: d.kind,
                display_name: d.display_name.clone(),
                variant: d.variant,
                channels: d.channels.clone(),
                parent,
                session: Some(session),
                handle: d.handle,
                state: ItemState::Declared,
                entity: None,
                frames_applied: 0,
                last_frame_id: 0,
                last_source_time_ns: 0,
                last_frame_s: now_s,
                timecode: None,
                dropped_out_of_order: 0,
                detail: ItemDetail::default(),
            },
        );
        Ok(Declared { guid, fresh: true })
    }

    /// Records an applied frame. Returns false (and counts `dropped_out_of_order`) for a
    /// non-increasing `frame_id`; the caller then does not apply it.
    pub fn note_frame(&mut self, guid: Guid, info: &FrameInfo, now_s: f64) -> bool {
        let Some(item) = self.items.get_mut(&guid) else {
            return false;
        };
        if item.frames_applied > 0 && info.frame_id <= item.last_frame_id {
            item.dropped_out_of_order += 1;
            return false;
        }
        item.frames_applied += 1;
        item.last_frame_id = info.frame_id;
        item.last_source_time_ns = info.source_time_ns;
        item.timecode = info.timecode;
        item.last_frame_s = now_s;
        item.state = ItemState::Active;
        true
    }

    /// `item_delete`: removes the item. Returns its record (the caller despawns the entity).
    pub fn delete(&mut self, session: SessionId, handle: u32) -> Option<ItemRecord> {
        let guid = self.handles.remove(&(session, handle))?;
        let mut item = self.items.remove(&guid)?;
        item.state = ItemState::Removed;
        // Children declared under it lose their parent link.
        for other in self.items.values_mut() {
            if other.parent == Some(guid) {
                other.parent = None;
            }
        }
        Some(item)
    }

    /// Removes an item whatever its owner (`dcc_feed.disconnect { despawn: true }`).
    pub fn purge(&mut self, guid: Guid) -> Option<ItemRecord> {
        let mut item = self.items.remove(&guid)?;
        self.handles.retain(|_, g| *g != guid);
        item.state = ItemState::Removed;
        Some(item)
    }

    /// The session closed: its items become `detached` (entity kept, still feed-owned).
    /// Returns their ids.
    pub fn detach_session(&mut self, session: SessionId) -> Vec<Guid> {
        self.handles.retain(|(s, _), _| *s != session);
        let mut out = Vec::new();
        for item in self.items.values_mut() {
            if item.session == Some(session) {
                item.session = None;
                item.handle = 0;
                item.state = ItemState::Detached;
                out.push(item.guid);
            }
        }
        out
    }

    /// Items with no frame for `stall_after_s` of engine time become `stalled`. Returns the
    /// newly stalled ids.
    pub fn update_stalls(&mut self, now_s: f64, stall_after_s: f64) -> Vec<Guid> {
        let mut out = Vec::new();
        for item in self.items.values_mut() {
            if item.state == ItemState::Active && now_s - item.last_frame_s >= stall_after_s {
                item.state = ItemState::Stalled;
                out.push(item.guid);
            }
        }
        out
    }

    /// Bakes an item: it leaves the table (the key is free again, spec § 2.4 rule 6).
    pub fn mark_baked(&mut self, guid: Guid) -> Option<ItemRecord> {
        let mut item = self.items.remove(&guid)?;
        self.handles.retain(|_, g| *g != guid);
        item.state = ItemState::Baked;
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declare(handle: u32, key: &str, kind: FeedKind, parent: u32) -> ItemDeclare {
        ItemDeclare {
            handle,
            key: key.to_owned(),
            kind,
            parent_handle: parent,
            display_name: key.to_owned(),
            variant: 0,
            channels: Vec::new(),
        }
    }

    fn info(frame_id: u64) -> FrameInfo {
        FrameInfo {
            frame_id,
            source_time_ns: frame_id * 10,
            timecode: None,
        }
    }

    #[test]
    fn declare_resolve_and_parent() {
        let mut t = ItemTable::new();
        let l = Limits::default();
        let root = t
            .declare(1, &declare(1, "Root", FeedKind::Transform, 0), &l, 0.0)
            .expect("root");
        let child = t
            .declare(1, &declare(2, "Cube", FeedKind::Mesh, 1), &l, 0.0)
            .expect("child");
        assert!(root.fresh && child.fresh);
        assert_eq!(root.guid, item_guid("Root"));
        assert_eq!(t.resolve(1, 2).expect("resolves"), child.guid);
        assert_eq!(t.get(child.guid).and_then(|i| i.parent), Some(root.guid));
        assert_eq!(t.resolve(1, 9).expect_err("unknown").code(), "unknown_item");
        assert_eq!(t.resolve(2, 1).expect_err("other session").code(), "unknown_item");
    }

    #[test]
    fn conflict_and_kind_mismatch() {
        let mut t = ItemTable::new();
        let l = Limits::default();
        t.declare(1, &declare(1, "Cube", FeedKind::Mesh, 0), &l, 0.0)
            .expect("first");
        let err = t
            .declare(2, &declare(5, "Cube", FeedKind::Mesh, 0), &l, 0.0)
            .expect_err("conflict");
        assert_eq!(err.code(), "item_conflict");
        assert!(!err.code.is_fatal());
        let err = t
            .declare(1, &declare(1, "Cube", FeedKind::Light, 0), &l, 0.0)
            .expect_err("kind");
        assert_eq!(err.code(), "kind_mismatch");
    }

    #[test]
    fn frames_must_increase() {
        let mut t = ItemTable::new();
        let l = Limits::default();
        let d = t
            .declare(1, &declare(1, "A", FeedKind::Transform, 0), &l, 0.0)
            .expect("a");
        assert!(t.note_frame(d.guid, &info(5), 0.1));
        assert!(!t.note_frame(d.guid, &info(5), 0.2));
        assert!(!t.note_frame(d.guid, &info(3), 0.2));
        assert!(t.note_frame(d.guid, &info(6), 0.3));
        let item = t.get(d.guid).expect("item");
        assert_eq!(item.frames_applied, 2);
        assert_eq!(item.dropped_out_of_order, 2);
        assert_eq!(item.last_frame_id, 6);
        assert_eq!(item.state, ItemState::Active);
    }

    #[test]
    fn stall_detach_and_rebind_keep_the_item() {
        let mut t = ItemTable::new();
        let l = Limits::default();
        let d = t
            .declare(1, &declare(1, "A", FeedKind::Transform, 0), &l, 0.0)
            .expect("a");
        t.note_frame(d.guid, &info(1), 0.0);
        assert!(t.update_stalls(0.49, 0.5).is_empty());
        assert_eq!(t.update_stalls(0.5, 0.5), vec![d.guid]);
        assert_eq!(t.get(d.guid).map(|i| i.state), Some(ItemState::Stalled));
        assert!(t.note_frame(d.guid, &info(2), 0.6));
        assert_eq!(t.get(d.guid).map(|i| i.state), Some(ItemState::Active));
        assert_eq!(t.detach_session(1), vec![d.guid]);
        let item = t.get(d.guid).expect("kept");
        assert_eq!((item.state, item.session), (ItemState::Detached, None));
        assert!(t.resolve(1, 1).is_err());
        // A new session declares the same key: same item, active again, frame ids restart.
        let again = t
            .declare(2, &declare(7, "A", FeedKind::Transform, 0), &l, 1.0)
            .expect("rebind");
        assert_eq!(
            again,
            Declared {
                guid: d.guid,
                fresh: false
            }
        );
        assert_eq!(t.get(d.guid).map(|i| i.state), Some(ItemState::Active));
        assert!(
            t.note_frame(d.guid, &info(1), 1.1),
            "frame ids restart with the new session"
        );
        assert_eq!(t.resolve(2, 7).expect("new handle"), d.guid);
    }

    #[test]
    fn delete_bake_and_limits() {
        let mut t = ItemTable::new();
        let l = Limits {
            max_items: 2,
            max_key_len: 4,
            ..Limits::default()
        };
        let a = t
            .declare(1, &declare(1, "A", FeedKind::Transform, 0), &l, 0.0)
            .expect("a");
        let b = t
            .declare(1, &declare(2, "B", FeedKind::Mesh, 1), &l, 0.0)
            .expect("b");
        assert_eq!(
            t.declare(1, &declare(3, "C", FeedKind::Mesh, 0), &l, 0.0)
                .expect_err("cap")
                .code(),
            "limit_exceeded"
        );
        assert_eq!(
            t.declare(2, &declare(3, "TOOLONG", FeedKind::Mesh, 0), &l, 0.0)
                .expect_err("key")
                .code(),
            "limit_exceeded"
        );
        let removed = t.delete(1, 1).expect("deleted");
        assert_eq!(removed.state, ItemState::Removed);
        assert_eq!(
            t.get(b.guid).and_then(|i| i.parent),
            None,
            "child loses its parent link"
        );
        let baked = t.mark_baked(b.guid).expect("baked");
        assert_eq!(baked.state, ItemState::Baked);
        assert!(t.is_empty());
        // The bake did not claim the key: a later declaration is a fresh item.
        let again = t
            .declare(1, &declare(4, "B", FeedKind::Mesh, 0), &l, 0.0)
            .expect("fresh");
        assert!(again.fresh);
        assert_ne!(a.guid, again.guid);
    }
}
