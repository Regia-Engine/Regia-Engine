// SPDX-License-Identifier: MPL-2.0
//! The local lobby backend: an in-memory lobby directory shared by every [`LocalLobbyBackend`]
//! made from one [`LocalLobbyHub`] (spec 89 § 1). Tests, play-in-editor with several clients and
//! builds without a platform use it.
//!
//! Deterministic: ordered maps, ids allocated in call order, no clock. Results of `create`,
//! `join` and `request_list` are queued for the caller's next [`LobbyBackend::poll`], the same
//! ordering a platform's callbacks give. Each member's event queue holds at most
//! `MAX_QUEUED_EVENTS` events (the oldest are dropped and counted).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{
    LeaveReason, LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyMember, LobbyQuery,
    LobbyServer, LobbySpec, MAX_KEYS, MemberId, RequestId, check_chat, check_key, check_spec, check_value,
    keys,
};

/// Events a member's queue holds before the oldest are dropped.
const MAX_QUEUED_EVENTS: usize = 1024;

#[derive(Debug, Default)]
struct HubState {
    next_lobby: u64,
    next_request: u64,
    lobbies: BTreeMap<LobbyId, LobbyInfo>,
    names: BTreeMap<MemberId, String>,
    queues: BTreeMap<MemberId, VecDeque<LobbyEvent>>,
    dropped: u64,
}

impl HubState {
    fn request(&mut self) -> RequestId {
        self.next_request += 1;
        RequestId(self.next_request)
    }

    fn push(&mut self, to: MemberId, event: LobbyEvent) {
        let queue = self.queues.entry(to).or_default();
        if queue.len() >= MAX_QUEUED_EVENTS {
            queue.pop_front();
            self.dropped += 1;
        }
        queue.push_back(event);
    }

    fn broadcast(&mut self, lobby: LobbyId, event: &LobbyEvent) {
        let members: Vec<MemberId> = self
            .lobbies
            .get(&lobby)
            .map(|l| l.members.iter().map(|m| m.id).collect())
            .unwrap_or_default();
        for m in members {
            self.push(m, event.clone());
        }
    }

    fn member_of(&self, lobby: LobbyId, me: MemberId) -> Result<&LobbyInfo, LobbyError> {
        match self.lobbies.get(&lobby) {
            Some(l) if l.has_member(me) => Ok(l),
            _ => Err(LobbyError::NotInLobby),
        }
    }

    fn owned(&mut self, lobby: LobbyId, me: MemberId) -> Result<&mut LobbyInfo, LobbyError> {
        match self.lobbies.get_mut(&lobby) {
            Some(l) if l.has_member(me) => {
                if l.owner == me {
                    Ok(l)
                } else {
                    Err(LobbyError::NotOwner)
                }
            }
            _ => Err(LobbyError::NotInLobby),
        }
    }

    /// Removes `member` from `lobby`, hands ownership to the earliest remaining member and deletes
    /// an empty lobby. Tells the member why and the others who left.
    fn remove(&mut self, lobby: LobbyId, member: MemberId, reason: LeaveReason) {
        let Some(info) = self.lobbies.get_mut(&lobby) else {
            return;
        };
        let Some(pos) = info.members.iter().position(|m| m.id == member) else {
            return;
        };
        info.members.remove(pos);
        info.member_count = info.members.len() as u32;
        let new_owner = if info.owner == member {
            info.members.first().map(|m| m.id)
        } else {
            None
        };
        if let Some(owner) = new_owner {
            info.owner = owner;
        }
        let empty = info.members.is_empty();
        self.push(member, LobbyEvent::Left { lobby, reason });
        if empty {
            self.lobbies.remove(&lobby);
            return;
        }
        self.broadcast(lobby, &LobbyEvent::MemberLeft { lobby, member });
        if let Some(owner) = new_owner {
            self.broadcast(lobby, &LobbyEvent::OwnerChanged { lobby, owner });
        }
    }
}

/// An in-memory lobby directory. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub struct LocalLobbyHub {
    state: Arc<Mutex<HubState>>,
}

impl LocalLobbyHub {
    /// An empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A backend for member `id` (display name `name`). `MemberId(0)` is reserved as invalid
    /// (Steam's invalid id) and is replaced by 1.
    pub fn member(&self, id: u64, name: &str) -> LocalLobbyBackend {
        let id = MemberId(id.max(1));
        let mut state = self.lock();
        state.names.insert(id, name.to_owned());
        state.queues.entry(id).or_default();
        LocalLobbyBackend {
            hub: self.clone(),
            me: id,
        }
    }

    /// Every lobby, members included, in id order (tests and the agent's dev view).
    pub fn lobbies(&self) -> Vec<LobbyInfo> {
        self.lock().lobbies.values().cloned().collect()
    }

    /// Removes `member` from `lobby` as the platform would on a kick (`Left { Kicked }`).
    pub fn kick(&self, lobby: LobbyId, member: MemberId) {
        self.lock().remove(lobby, member, LeaveReason::Kicked);
    }

    /// Drops `member` from every lobby as a lost platform connection would (`Left { Lost }`).
    pub fn drop_member(&self, member: MemberId) {
        let mut state = self.lock();
        let ids: Vec<LobbyId> = state
            .lobbies
            .values()
            .filter(|l| l.has_member(member))
            .map(|l| l.id)
            .collect();
        for id in ids {
            state.remove(id, member, LeaveReason::Lost);
        }
    }

    /// `to` accepted an invite from `from` to `lobby` (`InviteAccepted` on `to`'s next poll).
    pub fn invite(&self, from: MemberId, to: MemberId, lobby: LobbyId) {
        self.lock().push(to, LobbyEvent::InviteAccepted { lobby, from });
    }

    /// Events dropped because a member's queue was full.
    pub fn dropped_events(&self) -> u64 {
        self.lock().dropped
    }
}

/// One member's view of a [`LocalLobbyHub`].
#[derive(Debug, Clone)]
pub struct LocalLobbyBackend {
    hub: LocalLobbyHub,
    me: MemberId,
}

impl LocalLobbyBackend {
    /// The hub this backend talks to.
    pub fn hub(&self) -> &LocalLobbyHub {
        &self.hub
    }
}

fn set_entry(
    data: &mut BTreeMap<String, String>,
    key: &str,
    value: Option<&str>,
) -> Result<bool, LobbyError> {
    check_key(key)?;
    match value {
        Some(v) => {
            check_value(v)?;
            if !data.contains_key(key) && data.len() >= MAX_KEYS {
                return Err(LobbyError::TooManyKeys);
            }
            let changed = data.get(key).map(String::as_str) != Some(v);
            data.insert(key.to_owned(), v.to_owned());
            Ok(changed)
        }
        None => Ok(data.remove(key).is_some()),
    }
}

impl LobbyBackend for LocalLobbyBackend {
    fn name(&self) -> &'static str {
        "local"
    }

    fn local_member(&self) -> Option<MemberId> {
        Some(self.me)
    }

    fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError> {
        check_spec(spec)?;
        let mut state = self.hub.lock();
        let request = state.request();
        state.next_lobby += 1;
        let id = LobbyId(state.next_lobby);
        let name = state.names.get(&self.me).cloned().unwrap_or_default();
        let server = spec.data.get(keys::SERVER).and_then(|s| LobbyServer::from_key(s));
        state.lobbies.insert(
            id,
            LobbyInfo {
                id,
                owner: self.me,
                visibility: spec.visibility,
                max_members: spec.max_members,
                joinable: spec.joinable,
                members: vec![LobbyMember {
                    id: self.me,
                    name,
                    data: BTreeMap::new(),
                }],
                member_count: 1,
                data: spec.data.clone(),
                server,
            },
        );
        state.push(
            self.me,
            LobbyEvent::Created {
                request,
                result: Ok(id),
            },
        );
        Ok(request)
    }

    fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
        let mut state = self.hub.lock();
        let request = state.request();
        let name = state.names.get(&self.me).cloned().unwrap_or_default();
        let result = match state.lobbies.get_mut(&lobby) {
            None => Err(LobbyError::NotFound),
            Some(l) if l.has_member(self.me) => Ok(false),
            Some(l) if !l.joinable || l.member_count >= l.max_members => Err(LobbyError::Full),
            Some(l) => {
                l.members.push(LobbyMember {
                    id: self.me,
                    name,
                    data: BTreeMap::new(),
                });
                l.member_count = l.members.len() as u32;
                Ok(true)
            }
        };
        match result {
            Ok(fresh) => {
                if fresh {
                    let others: Vec<MemberId> = state
                        .lobbies
                        .get(&lobby)
                        .map(|l| l.members.iter().map(|m| m.id).filter(|m| *m != self.me).collect())
                        .unwrap_or_default();
                    for m in others {
                        state.push(
                            m,
                            LobbyEvent::MemberJoined {
                                lobby,
                                member: self.me,
                            },
                        );
                    }
                }
                state.push(
                    self.me,
                    LobbyEvent::Entered {
                        request,
                        result: Ok(lobby),
                    },
                );
            }
            Err(e) => state.push(
                self.me,
                LobbyEvent::Entered {
                    request,
                    result: Err(e),
                },
            ),
        }
        Ok(request)
    }

    fn leave(&self, lobby: LobbyId) {
        self.hub.lock().remove(lobby, self.me, LeaveReason::Left);
    }

    fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError> {
        query.validate()?;
        let mut state = self.hub.lock();
        let request = state.request();
        let result = query.evaluate(state.lobbies.values());
        state.push(
            self.me,
            LobbyEvent::List {
                request,
                result: Ok(result),
            },
        );
        Ok(request)
    }

    fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo> {
        self.hub.lock().member_of(lobby, self.me).ok().cloned()
    }

    fn still_in(&self, lobby: LobbyId) -> bool {
        self.hub.lock().member_of(lobby, self.me).is_ok()
    }

    fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
        let mut state = self.hub.lock();
        let info = state.owned(lobby, self.me)?;
        let changed = set_entry(&mut info.data, key, value)?;
        if key == keys::SERVER {
            info.server = value.and_then(LobbyServer::from_key);
        }
        if changed {
            state.broadcast(lobby, &LobbyEvent::DataChanged { lobby, member: None });
        }
        Ok(())
    }

    fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
        let mut state = self.hub.lock();
        state.member_of(lobby, self.me)?;
        let me = self.me;
        let changed = match state
            .lobbies
            .get_mut(&lobby)
            .and_then(|l| l.members.iter_mut().find(|m| m.id == me))
        {
            Some(member) => set_entry(&mut member.data, key, value)?,
            None => return Err(LobbyError::NotInLobby),
        };
        if changed {
            state.broadcast(
                lobby,
                &LobbyEvent::DataChanged {
                    lobby,
                    member: Some(me),
                },
            );
        }
        Ok(())
    }

    fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError> {
        let mut state = self.hub.lock();
        let info = state.owned(lobby, self.me)?;
        if info.joinable != joinable {
            info.joinable = joinable;
            state.broadcast(lobby, &LobbyEvent::DataChanged { lobby, member: None });
        }
        Ok(())
    }

    fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError> {
        let mut state = self.hub.lock();
        let info = state.owned(lobby, self.me)?;
        info.server = Some(server);
        info.data.insert(keys::SERVER.to_owned(), server.to_key());
        state.broadcast(lobby, &LobbyEvent::ServerSet { lobby, server });
        Ok(())
    }

    fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError> {
        check_chat(text)?;
        let mut state = self.hub.lock();
        state.member_of(lobby, self.me)?;
        state.broadcast(
            lobby,
            &LobbyEvent::Chat {
                lobby,
                from: self.me,
                text: text.to_owned(),
            },
        );
        Ok(())
    }

    fn poll(&self) -> Vec<LobbyEvent> {
        self.hub
            .lock()
            .queues
            .get_mut(&self.me)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobby::{Compare, LobbyVisibility};

    fn created(events: &[LobbyEvent]) -> LobbyId {
        events
            .iter()
            .find_map(|e| match e {
                LobbyEvent::Created { result: Ok(id), .. } => Some(*id),
                _ => None,
            })
            .expect("created")
    }

    #[test]
    fn create_join_leave_and_owner_hand_off() {
        let hub = LocalLobbyHub::new();
        let ann = hub.member(1, "Ann");
        let bob = hub.member(2, "Bob");
        let cy = hub.member(3, "Cy");
        let req = ann
            .create(&LobbySpec {
                max_members: 3,
                ..LobbySpec::default()
            })
            .expect("create");
        let ev = ann.poll();
        assert_eq!(
            ev,
            vec![LobbyEvent::Created {
                request: req,
                result: Ok(LobbyId(1))
            }]
        );
        let lobby = created(&ev);
        bob.join(lobby).expect("join");
        cy.join(lobby).expect("join");
        assert_eq!(
            ann.poll(),
            vec![
                LobbyEvent::MemberJoined {
                    lobby,
                    member: MemberId(2)
                },
                LobbyEvent::MemberJoined {
                    lobby,
                    member: MemberId(3)
                },
            ]
        );
        let info = bob.lobby(lobby).expect("in lobby");
        assert_eq!(info.member_count, 3);
        assert_eq!(
            info.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["Ann", "Bob", "Cy"]
        );
        // A fourth member finds it full.
        let dee = hub.member(4, "Dee");
        let r = dee.join(lobby).expect("request");
        assert_eq!(
            dee.poll(),
            vec![LobbyEvent::Entered {
                request: r,
                result: Err(LobbyError::Full)
            }]
        );
        // The owner leaves: the earliest remaining member owns it.
        bob.poll();
        cy.poll();
        ann.leave(lobby);
        assert_eq!(
            ann.poll(),
            vec![LobbyEvent::Left {
                lobby,
                reason: LeaveReason::Left
            }]
        );
        assert_eq!(
            cy.poll(),
            vec![
                LobbyEvent::MemberLeft {
                    lobby,
                    member: MemberId(1)
                },
                LobbyEvent::OwnerChanged {
                    lobby,
                    owner: MemberId(2)
                },
            ]
        );
        assert_eq!(bob.lobby(lobby).map(|l| l.owner), Some(MemberId(2)));
        assert!(ann.lobby(lobby).is_none());
        // Everyone leaves: the lobby is gone.
        bob.leave(lobby);
        cy.leave(lobby);
        assert!(hub.lobbies().is_empty());
        let r = dee.join(lobby).expect("request");
        assert_eq!(
            dee.poll(),
            vec![LobbyEvent::Entered {
                request: r,
                result: Err(LobbyError::NotFound)
            }]
        );
    }

    #[test]
    fn data_is_owner_only_and_member_data_is_per_member() {
        let hub = LocalLobbyHub::new();
        let ann = hub.member(1, "Ann");
        let bob = hub.member(2, "Bob");
        ann.create(&LobbySpec::default()).expect("create");
        let lobby = created(&ann.poll());
        bob.join(lobby).expect("join");
        ann.poll();
        bob.poll();
        assert_eq!(bob.set_data(lobby, "map", Some("x")), Err(LobbyError::NotOwner));
        ann.set_data(lobby, "map", Some("harbor")).expect("owner sets");
        // Same value again: no event.
        ann.set_data(lobby, "map", Some("harbor")).expect("owner sets");
        assert_eq!(bob.poll(), vec![LobbyEvent::DataChanged { lobby, member: None }]);
        bob.set_member_data(lobby, keys::READY, Some("1"))
            .expect("member data");
        assert_eq!(
            ann.poll(),
            vec![
                LobbyEvent::DataChanged { lobby, member: None },
                LobbyEvent::DataChanged {
                    lobby,
                    member: Some(MemberId(2))
                }
            ]
        );
        let info = ann.lobby(lobby).expect("lobby");
        assert_eq!(info.data["map"], "harbor");
        assert!(!info.all_ready(), "Ann is not ready");
        ann.set_member_data(lobby, keys::READY, Some("1")).expect("ready");
        assert!(ann.lobby(lobby).expect("lobby").all_ready());
        ann.set_data(lobby, "map", None).expect("delete");
        assert!(!ann.lobby(lobby).expect("lobby").data.contains_key("map"));
        for i in 0..MAX_KEYS {
            ann.set_data(lobby, &format!("k{i}"), Some("v")).expect("room");
        }
        assert_eq!(
            ann.set_data(lobby, "one_more", Some("v")),
            Err(LobbyError::TooManyKeys)
        );
        let outsider = hub.member(9, "Out");
        assert_eq!(
            outsider.set_member_data(lobby, "x", Some("1")),
            Err(LobbyError::NotInLobby)
        );
    }

    #[test]
    fn list_server_chat_invites_kicks_and_losses() {
        let hub = LocalLobbyHub::new();
        let ann = hub.member(1, "Ann");
        let bob = hub.member(2, "Bob");
        let mut spec = LobbySpec::default();
        spec.data.insert("mode".into(), "duel".into());
        ann.create(&spec).expect("create");
        spec.visibility = LobbyVisibility::Private;
        ann.create(&spec).expect("create hidden");
        let lobby = created(&ann.poll());
        let r = bob
            .request_list(&LobbyQuery::default().with_string("mode", Compare::Eq, "duel"))
            .expect("list");
        match bob.poll().as_slice() {
            [
                LobbyEvent::List {
                    request,
                    result: Ok(list),
                },
            ] => {
                assert_eq!(*request, r);
                assert_eq!(
                    list.iter().map(|l| l.id).collect::<Vec<_>>(),
                    [lobby],
                    "private not listed"
                );
                assert!(list[0].members.is_empty(), "listings carry counts only");
                assert_eq!(list[0].member_count, 1);
            }
            other => panic!("{other:?}"),
        }
        bob.join(lobby).expect("join");
        bob.poll();
        ann.poll();
        assert_eq!(bob.send_chat(lobby, ""), Err(LobbyError::ChatInvalid));
        bob.send_chat(lobby, "gl hf").expect("chat");
        let server = LobbyServer::Steam(42);
        assert_eq!(bob.set_server(lobby, server), Err(LobbyError::NotOwner));
        ann.set_server(lobby, server).expect("launch");
        let chat = LobbyEvent::Chat {
            lobby,
            from: MemberId(2),
            text: "gl hf".into(),
        };
        let set = LobbyEvent::ServerSet { lobby, server };
        assert_eq!(bob.poll(), vec![chat.clone(), set.clone()]);
        assert_eq!(ann.poll(), vec![chat, set]);
        assert_eq!(bob.lobby(lobby).and_then(|l| l.server), Some(server));
        assert_eq!(bob.lobby(lobby).expect("lobby").data[keys::SERVER], "steam:42");
        hub.invite(MemberId(1), MemberId(2), lobby);
        assert_eq!(
            bob.poll(),
            vec![LobbyEvent::InviteAccepted {
                lobby,
                from: MemberId(1)
            }]
        );
        hub.kick(lobby, MemberId(2));
        assert_eq!(
            bob.poll(),
            vec![LobbyEvent::Left {
                lobby,
                reason: LeaveReason::Kicked
            }]
        );
        hub.drop_member(MemberId(1));
        assert_eq!(
            ann.poll(),
            vec![
                LobbyEvent::MemberLeft {
                    lobby,
                    member: MemberId(2)
                },
                LobbyEvent::Left {
                    lobby,
                    reason: LeaveReason::Lost
                },
                LobbyEvent::Left {
                    lobby: LobbyId(2),
                    reason: LeaveReason::Lost
                },
            ]
        );
        assert!(hub.lobbies().is_empty());
    }

    #[test]
    fn queues_are_bounded() {
        let hub = LocalLobbyHub::new();
        let ann = hub.member(1, "Ann");
        for _ in 0..(MAX_QUEUED_EVENTS + 5) {
            ann.request_list(&LobbyQuery::default()).expect("list");
        }
        assert_eq!(ann.poll().len(), MAX_QUEUED_EVENTS);
        assert_eq!(hub.dropped_events(), 5);
    }
}
