//! Management of user relationships (Friends, Pending Requests, Blocked Users).
//!
//! Submodules:
//! - [`data`]: The state struct [`RelationshipsState`], the core reducer [`RelationshipsStore`],
//!   server synchronization, live gateway event application, and journaled snapshot coalescing.
//! - [`view`]: UI components for tabs, search filtering, virtualized user rows, and modals.

pub mod data;
pub mod view;

pub use data::RelationshipsState;

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::data::{FetchCompletionOutcome, MAX_JOURNAL_SIZE, RelationshipsStore};
    use crate::discord::relationships::*;

    fn make_test_user(
        id_num: u64,
        username: &str,
        disc: Option<&str>,
        global_name: Option<&str>,
    ) -> RelationshipUser {
        RelationshipUser {
            id: Id::new(id_num),
            username: username.to_owned(),
            discriminator: disc.map(|d| d.to_owned()),
            global_name: global_name.map(|g| g.to_owned()),
            avatar: None,
            bot: false,
            public_flags: None,
        }
    }

    fn make_relationship(id_num: u64, kind: RelationshipType, username: &str) -> Relationship {
        Relationship {
            id: Id::new(id_num),
            kind,
            user: make_test_user(id_num, username, None, None),
            nickname: None,
            user_ignored: false,
            is_spam_request: false,
            stranger_request: false,
            since: None,
        }
    }

    #[test]
    fn test_decoding_unknown_relationship() {
        let json_data = r#"[
            {"id":"100","type":0,"user":{"id":"100","username":"none_user"}},
            {"id":"101","type":1,"user":{"id":"101","username":"friend_user"}},
            {"id":"102","type":2,"user":{"id":"102","username":"blocked_user"}},
            {"id":"103","type":3,"user":{"id":"103","username":"incoming_user"}},
            {"id":"104","type":4,"user":{"id":"104","username":"outgoing_user"}},
            {"id":"105","type":5,"user":{"id":"105","username":"implicit_user"}},
            {"id":"106","type":6,"user":{"id":"106","username":"suggestion_user"}},
            {"id":"107","type":77,"user":{"id":"107","username":"future_type_user"}}
        ]"#;

        let rels: Vec<Relationship> = serde_json::from_str(json_data).unwrap();
        assert_eq!(rels.len(), 8);
        assert_eq!(rels[0].kind, RelationshipType::None);
        assert_eq!(rels[1].kind, RelationshipType::Friend);
        assert_eq!(rels[2].kind, RelationshipType::Blocked);
        assert_eq!(rels[3].kind, RelationshipType::IncomingRequest);
        assert_eq!(rels[4].kind, RelationshipType::OutgoingRequest);
        assert_eq!(rels[5].kind, RelationshipType::Implicit);
        assert_eq!(rels[6].kind, RelationshipType::Suggestion);
        assert_eq!(rels[7].kind, RelationshipType::Unknown(77));

        assert!(rels[1].kind.is_friend());
        assert!(rels[2].kind.is_blocked());
        assert!(rels[3].kind.is_pending());
        assert!(rels[4].kind.is_pending());
        assert!(!rels[7].kind.is_friend());
    }

    #[test]
    fn test_identidad_id_and_display_name() {
        let u1 = make_test_user(123456789, "wumpus", None, Some("Friendly Wumpus"));
        assert_eq!(u1.id.get(), 123456789);
        assert_eq!(u1.display_name(), "Friendly Wumpus");
        assert_eq!(u1.tag(), "@wumpus");

        let u2 = make_test_user(987654321, "classic", Some("1337"), None);
        assert_eq!(u2.id.get(), 987654321);
        assert_eq!(u2.display_name(), "classic");
        assert_eq!(u2.tag(), "classic#1337");

        let mut rel = make_relationship(123456789, RelationshipType::Friend, "wumpus");
        rel.user = u1;
        assert_eq!(rel.display_name(), "Friendly Wumpus");

        rel.nickname = Some("My Best Pal".to_owned());
        assert_eq!(rel.display_name(), "My Best Pal");
    }

    #[test]
    fn test_initial_fetch_coalesces_with_live_events_both_survive() {
        let mut store = RelationshipsStore::default();

        // 1. Initial fetch launched
        let fetch_gen = store.start_load().expect("load must start");
        assert!(store.loading);
        assert!(!store.loaded);

        // 2. While HTTP fetch is pending, a new live event arrives
        let live_friend_b = make_relationship(202, RelationshipType::Friend, "live_user_b");
        store.apply_event(&RelationshipEvent::Add(live_friend_b.clone()));

        assert_eq!(store.relationships.len(), 1);
        assert_eq!(store.relationships[0].id.get(), 202);
        assert_eq!(store.pending_event_journal.len(), 1);

        // 3. Delayed HTTP snapshot completes carrying friend_A and friend_C
        let snapshot = vec![
            make_relationship(101, RelationshipType::Friend, "snapshot_user_a"),
            make_relationship(303, RelationshipType::Friend, "snapshot_user_c"),
        ];

        let outcome = store.handle_fetch_completion(fetch_gen, Ok(snapshot));
        assert_eq!(outcome, FetchCompletionOutcome::Applied);

        // 4. Verification:
        assert!(!store.loading);
        assert!(store.loaded);
        assert_eq!(store.pending_event_journal.len(), 0);

        // Both snapshot friends (A, C) and live friend (B) survive!
        assert_eq!(store.relationships.len(), 3);
        let ids: Vec<u64> = store.relationships.iter().map(|r| r.id.get()).collect();
        assert!(ids.contains(&101));
        assert!(ids.contains(&202));
        assert!(ids.contains(&303));
    }

    #[test]
    fn test_live_remove_does_not_resurrect_from_snapshot() {
        let mut store = RelationshipsStore {
            relationships: vec![
                make_relationship(101, RelationshipType::Friend, "alice"),
                make_relationship(102, RelationshipType::Friend, "bob"),
            ],
            loaded: true,
            ..RelationshipsStore::default()
        };

        // 1. Background refresh starts
        let fetch_gen = store.start_refresh().expect("refresh must start");
        assert!(store.refreshing);

        // 2. User removes alice live while refresh is in-flight
        store.apply_event(&RelationshipEvent::Remove(RelationshipRemove {
            id: Id::new(101),
            kind: RelationshipType::Friend,
            nickname: None,
            user_ignored: false,
        }));
        assert_eq!(store.relationships.len(), 1);
        assert_eq!(store.relationships[0].id.get(), 102);

        // 3. Delayed server snapshot arrives, which still contained alice before deletion
        let stale_server_snapshot = vec![
            make_relationship(101, RelationshipType::Friend, "alice"),
            make_relationship(102, RelationshipType::Friend, "bob"),
            make_relationship(103, RelationshipType::Friend, "charlie"),
        ];

        let outcome = store.handle_fetch_completion(fetch_gen, Ok(stale_server_snapshot));
        assert_eq!(outcome, FetchCompletionOutcome::Applied);

        // 4. Verification:
        assert!(!store.refreshing);

        // Alice was removed live and MUST NOT be resurrected from the snapshot!
        let ids: Vec<u64> = store.relationships.iter().map(|r| r.id.get()).collect();
        assert!(
            !ids.contains(&101),
            "alice must not resurrect from stale snapshot"
        );
        assert!(ids.contains(&102));
        assert!(ids.contains(&103));
        assert_eq!(store.relationships.len(), 2);
    }

    #[test]
    fn test_journal_overflow_at_256_plus_remove_257_does_not_resurrect() {
        let mut store = RelationshipsStore::default();
        let user_257_id = Id::new(9999);

        // Initial relationships including user 257
        store.relationships = vec![make_relationship(
            9999,
            RelationshipType::Friend,
            "user_257",
        )];
        store.loaded = true;

        let fetch_gen = store.start_refresh().expect("refresh started");
        assert!(store.refreshing);

        // Fill journal up to MAX_JOURNAL_SIZE (256)
        for i in 1..=MAX_JOURNAL_SIZE as u64 {
            let rel = make_relationship(i, RelationshipType::Friend, &format!("user_{i}"));
            store.apply_event(&RelationshipEvent::Add(rel));
        }
        assert_eq!(store.pending_event_journal.len(), MAX_JOURNAL_SIZE);
        assert!(!store.journal_overflowed);

        // Now remove 257th user live while at cap
        store.apply_event(&RelationshipEvent::Remove(RelationshipRemove {
            id: user_257_id,
            kind: RelationshipType::Friend,
            nickname: None,
            user_ignored: false,
        }));

        // User 257 is immediately removed from live state
        assert!(!store.relationships.iter().any(|r| r.id == user_257_id));
        // Cap overflow is safely flagged
        assert!(store.journal_overflowed);

        // Server returns stale snapshot containing user 257
        let stale_snapshot = vec![make_relationship(
            9999,
            RelationshipType::Friend,
            "user_257",
        )];
        let outcome = store.handle_fetch_completion(fetch_gen, Ok(stale_snapshot));

        // Snapshot MUST NOT be applied!
        assert_eq!(outcome, FetchCompletionOutcome::NeedsResync);
        assert!(!store.refreshing);
        assert!(!store.loading);
        assert!(!store.journal_overflowed);

        // User 257 MUST NOT resurrect!
        assert!(
            !store.relationships.iter().any(|r| r.id == user_257_id),
            "user 257 must never resurrect on journal overflow"
        );
    }

    #[test]
    fn test_coalescence_multiple_updates_same_id_at_cap() {
        let mut store = RelationshipsStore::default();
        let fetch_gen = store.start_refresh().expect("refresh started");

        // Fill journal to 256 entries
        for i in 1..=MAX_JOURNAL_SIZE as u64 {
            let rel = make_relationship(i, RelationshipType::Friend, &format!("user_{i}"));
            store.apply_event(&RelationshipEvent::Add(rel));
        }
        assert_eq!(store.pending_event_journal.len(), MAX_JOURNAL_SIZE);
        assert!(!store.journal_overflowed);

        // Re-update user #1 multiple times while at cap
        let mut updated_user_1 = make_relationship(1, RelationshipType::Blocked, "user_1_blocked");
        store.apply_event(&RelationshipEvent::Add(updated_user_1.clone()));
        updated_user_1.nickname = Some("Nickname1".to_owned());
        store.apply_event(&RelationshipEvent::Add(updated_user_1));

        // Existing ID in journal is updated in-place: length remains 256, no overflow!
        assert_eq!(store.pending_event_journal.len(), MAX_JOURNAL_SIZE);
        assert!(!store.journal_overflowed);

        // Completion succeeds and applies coalesced updates
        let snapshot = vec![make_relationship(1, RelationshipType::Friend, "user_1")];
        let outcome = store.handle_fetch_completion(fetch_gen, Ok(snapshot));
        assert_eq!(outcome, FetchCompletionOutcome::Applied);

        let user_1 = store
            .relationships
            .iter()
            .find(|r| r.id.get() == 1)
            .unwrap();
        assert_eq!(user_1.kind, RelationshipType::Blocked);
        assert_eq!(user_1.nickname.as_deref(), Some("Nickname1"));
    }

    #[test]
    fn test_prior_fetch_completion_never_clears_newer_fetch() {
        let mut store = RelationshipsStore::default();

        // 1. Fetch 1 starts
        let fetch_gen1 = store.start_refresh().expect("first refresh");
        assert!(store.refreshing);

        // 2. Fetch 2 supersedes Fetch 1
        store.refreshing = false;
        let fetch_gen2 = store.start_refresh().expect("second refresh");
        assert_ne!(fetch_gen1, fetch_gen2);
        assert!(store.refreshing);

        // 3. Delayed completion from Fetch 1 arrives
        let snapshot1 = vec![make_relationship(1, RelationshipType::Friend, "user1")];
        let outcome1 = store.handle_fetch_completion(fetch_gen1, Ok(snapshot1));

        // Must be rejected by the generation fence
        assert_eq!(outcome1, FetchCompletionOutcome::Stale);
        assert!(
            store.refreshing,
            "prior completion must never clear newer fetch state"
        );
        assert_eq!(store.relationships.len(), 0);

        // 4. Completion of Fetch 2 arrives
        let snapshot2 = vec![make_relationship(2, RelationshipType::Friend, "user2")];
        let outcome2 = store.handle_fetch_completion(fetch_gen2, Ok(snapshot2));
        assert_eq!(outcome2, FetchCompletionOutcome::Applied);
        assert!(!store.refreshing);
        assert_eq!(store.relationships.len(), 1);
        assert_eq!(store.relationships[0].id.get(), 2);
    }

    #[test]
    fn test_mutation_session_fence_and_failure_preservation() {
        let mut store = RelationshipsStore {
            relationships: vec![
                make_relationship(1, RelationshipType::Friend, "user1"),
                make_relationship(2, RelationshipType::Friend, "user2"),
            ],
            ..RelationshipsStore::default()
        };

        let target_user = Id::new(2);
        store.busy_users.insert(target_user);
        let active_session = store.session_generation;

        // Session reset happens (e.g. logout)
        store.reset();
        assert_eq!(store.relationships.len(), 0);
        assert_eq!(store.busy_users.len(), 0);

        // Outdated mutation completion from previous session rejected
        let accepted = store.handle_mutation_completion(
            active_session,
            target_user,
            Err("401 Unauthorized".to_string()),
        );
        assert!(!accepted);
        assert_eq!(store.row_errors.len(), 0);

        // In new session, failure properly records error without altering relationships
        store.relationships = vec![make_relationship(3, RelationshipType::Friend, "user3")];
        let new_target = Id::new(3);
        store.busy_users.insert(new_target);
        let current_session = store.session_generation;

        let accepted = store.handle_mutation_completion(
            current_session,
            new_target,
            Err("500 Internal Error".to_string()),
        );
        assert!(accepted);
        assert!(!store.busy_users.contains(&new_target));
        assert_eq!(
            store.row_errors.get(&new_target).unwrap(),
            "500 Internal Error"
        );
        assert_eq!(store.relationships.len(), 1);
        assert_eq!(store.relationships[0].id.get(), 3);
    }

    #[test]
    fn test_replace_snapshot() {
        let mut store = RelationshipsStore::default();
        let snapshot = vec![
            make_relationship(10, RelationshipType::Friend, "friend1"),
            make_relationship(11, RelationshipType::Blocked, "blocked1"),
        ];

        let prev_gen = store.fetch_generation;
        store.replace_snapshot(snapshot);

        assert!(store.loaded);
        assert!(!store.loading);
        assert_eq!(store.relationships.len(), 2);
        assert!(store.is_blocked(Id::new(11)));
        assert!(!store.is_blocked(Id::new(10)));
        assert_eq!(store.fetch_generation, prev_gen + 1);
    }
}
