// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::{BTreeMap, BTreeSet};

use matrix_sdk_common::{
    deserialized_responses::{AmbiguityChange, MemberEvent},
    events::room::member::MembershipState,
    identifiers::{EventId, RoomId, UserId},
};

use tracing::trace;

use crate::Store;

use super::{Result, StateChanges};

#[derive(Clone, Debug)]
pub struct AmbiguityCache {
    pub store: Store,
    pub cache: BTreeMap<RoomId, BTreeMap<String, BTreeSet<UserId>>>,
    pub changes: BTreeMap<RoomId, BTreeMap<EventId, AmbiguityChange>>,
}

#[derive(Clone, Debug)]
struct AmbiguityMap {
    display_name: String,
    users: BTreeSet<UserId>,
}

impl AmbiguityMap {
    fn remove(&mut self, user_id: &UserId) -> Option<UserId> {
        self.users.remove(user_id);

        if self.user_count() == 1 {
            self.users.iter().next().cloned()
        } else {
            None
        }
    }

    fn add(&mut self, user_id: UserId) -> Option<UserId> {
        let ambiguous_user = if self.user_count() == 1 {
            self.users.iter().next().cloned()
        } else {
            None
        };

        self.users.insert(user_id);

        ambiguous_user
    }

    fn user_count(&self) -> usize {
        self.users.len()
    }

    fn is_ambiguous(&self) -> bool {
        self.user_count() > 1
    }
}

impl AmbiguityCache {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            cache: BTreeMap::new(),
            changes: BTreeMap::new(),
        }
    }

    pub async fn handle_event(
        &mut self,
        changes: &StateChanges,
        room_id: &RoomId,
        member_event: &MemberEvent,
    ) -> Result<()> {
        // Synapse seems to have a bug where it puts the same event into the
        // state and the timeline sometimes.
        //
        // Since our state, e.g. the old display name, already ended up inside
        // the state changes and we're pulling stuff out of the cache if it's
        // there calculating this twice for the same event will result in an
        // incorrect AmbiguityChange overwriting the correct one. In other
        // words, this method is not idempotent so we make it by ignoring
        // duplicate events.
        if self
            .changes
            .get(room_id)
            .map(|c| c.contains_key(&member_event.event_id))
            .unwrap_or(false)
        {
            return Ok(());
        }

        let (mut old_map, mut new_map) = self.get(changes, room_id, member_event).await?;

        let display_names_same = match (&old_map, &new_map) {
            (Some(a), Some(b)) => a.display_name == b.display_name,
            _ => false,
        };

        if display_names_same {
            return Ok(());
        }

        let disambiguated_member = old_map
            .as_mut()
            .and_then(|o| o.remove(&member_event.state_key));
        let ambiguated_member = new_map
            .as_mut()
            .and_then(|n| n.add(member_event.state_key.clone()));
        let ambiguous = new_map.as_ref().map(|n| n.is_ambiguous()).unwrap_or(false);

        self.update(room_id, old_map, new_map);

        let change = AmbiguityChange {
            disambiguated_member,
            ambiguated_member,
            member_ambiguous: ambiguous,
        };

        trace!(
            "Handling display name ambiguity for {}: {:#?}",
            member_event.state_key,
            change
        );

        self.add_change(room_id, member_event.event_id.clone(), change);

        Ok(())
    }

    fn update(
        &mut self,
        room_id: &RoomId,
        old_map: Option<AmbiguityMap>,
        new_map: Option<AmbiguityMap>,
    ) {
        let entry = self
            .cache
            .entry(room_id.clone())
            .or_insert_with(BTreeMap::new);

        if let Some(old) = old_map {
            entry.insert(old.display_name, old.users);
        }

        if let Some(new) = new_map {
            entry.insert(new.display_name, new.users);
        }
    }

    fn add_change(&mut self, room_id: &RoomId, event_id: EventId, change: AmbiguityChange) {
        self.changes
            .entry(room_id.clone())
            .or_insert_with(BTreeMap::new)
            .insert(event_id, change);
    }

    async fn get(
        &mut self,
        changes: &StateChanges,
        room_id: &RoomId,
        member_event: &MemberEvent,
    ) -> Result<(Option<AmbiguityMap>, Option<AmbiguityMap>)> {
        use MembershipState::*;

        let old_event = if let Some(m) = changes
            .members
            .get(room_id)
            .and_then(|m| m.get(&member_event.state_key))
        {
            Some(m.clone())
        } else if let Some(m) = self
            .store
            .get_member_event(room_id, &member_event.state_key)
            .await?
        {
            Some(m)
        } else {
            None
        };

        let old_display_name = if let Some(event) = old_event {
            if matches!(event.content.membership, Join | Invite) {
                let display_name = if let Some(d) = changes
                    .profiles
                    .get(room_id)
                    .and_then(|p| p.get(&member_event.state_key))
                    .and_then(|p| p.displayname.as_deref())
                {
                    Some(d.to_string())
                } else if let Some(d) = self
                    .store
                    .get_profile(room_id, &member_event.state_key)
                    .await?
                    .and_then(|c| c.displayname)
                {
                    Some(d)
                } else {
                    event.content.displayname.clone()
                };

                Some(display_name.unwrap_or_else(|| event.state_key.localpart().to_string()))
            } else {
                None
            }
        } else {
            None
        };

        let old_map = if let Some(old_name) = old_display_name.as_deref() {
            let old_display_name_map = if let Some(u) = self
                .cache
                .entry(room_id.clone())
                .or_insert_with(BTreeMap::new)
                .get(old_name)
            {
                u.clone()
            } else {
                self.store
                    .get_users_with_display_name(&room_id, &old_name)
                    .await?
            };

            Some(AmbiguityMap {
                display_name: old_name.to_string(),
                users: old_display_name_map,
            })
        } else {
            None
        };

        let new_map = if matches!(member_event.content.membership, Join | Invite) {
            let new = member_event
                .content
                .displayname
                .as_deref()
                .unwrap_or_else(|| member_event.state_key.localpart());

            // We don't allow other users to set the display name, so if we have
            // a more trusted version of the display name use that.
            let new_display_name = if member_event.sender.as_str() == member_event.state_key {
                new
            } else if let Some(old) = old_display_name.as_deref() {
                old
            } else {
                new
            };

            let new_display_name_map = if let Some(u) = self
                .cache
                .entry(room_id.clone())
                .or_insert_with(BTreeMap::new)
                .get(new_display_name)
            {
                u.clone()
            } else {
                self.store
                    .get_users_with_display_name(&room_id, &new_display_name)
                    .await?
            };

            Some(AmbiguityMap {
                display_name: new_display_name.to_string(),
                users: new_display_name_map,
            })
        } else {
            None
        };

        Ok((old_map, new_map))
    }
}
