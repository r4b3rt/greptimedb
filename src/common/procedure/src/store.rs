// Copyright 2023 Greptime Team
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

use std::collections::HashMap;
use std::fmt;

use common_telemetry::logging;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;

use crate::error::{Result, ToJsonSnafu};
use crate::store::state_store::StateStoreRef;
use crate::{BoxedProcedure, ProcedureId};

mod state_store;

/// Serialized data of a procedure.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ProcedureMessage {
    /// Type name of the procedure. The procedure framework also use the type name to
    /// find a loader to load the procedure.
    type_name: String,
    /// The data of the procedure.
    data: String,
    /// Parent procedure id.
    parent_id: Option<ProcedureId>,
}

/// Procedure storage layer.
#[derive(Clone)]
struct ProcedureStore(StateStoreRef);

impl ProcedureStore {
    /// Dump the `procedure` to the storage.
    async fn store_procedure(
        &self,
        procedure_id: ProcedureId,
        step: u32,
        procedure: &BoxedProcedure,
        parent_id: Option<ProcedureId>,
    ) -> Result<()> {
        let type_name = procedure.type_name();
        let data = procedure.dump()?;

        let message = ProcedureMessage {
            type_name: type_name.to_string(),
            data,
            parent_id,
        };
        let key = ParsedKey {
            procedure_id,
            step,
            is_committed: false,
        }
        .to_string();
        let value = serde_json::to_string(&message).context(ToJsonSnafu)?;

        self.0.put(&key, value.into_bytes()).await?;

        Ok(())
    }

    /// Write commit flag to the storage.
    async fn commit_procedure(&self, procedure_id: ProcedureId, step: u32) -> Result<()> {
        let key = ParsedKey {
            procedure_id,
            step,
            is_committed: true,
        }
        .to_string();
        self.0.put(&key, Vec::new()).await?;

        Ok(())
    }

    /// Load uncommitted procedures from the storage.
    async fn load_messages(&self) -> Result<HashMap<ProcedureId, ProcedureMessage>> {
        let mut messages = HashMap::new();
        // Track the key-value pair by procedure id.
        let mut procedure_key_values: HashMap<_, (ParsedKey, Vec<u8>)> = HashMap::new();

        // Scan all procedures.
        let mut key_values = self.0.walk_top_down("/").await?;
        while let Some((key, value)) = key_values.try_next().await? {
            let Some(curr_key) = ParsedKey::parse_str(&key) else {
                logging::warn!("Unknown key while loading procedures, key: {}", key);
                continue;
            };

            if let Some(entry) = procedure_key_values.get_mut(&curr_key.procedure_id) {
                if entry.0.step < curr_key.step {
                    entry.0 = curr_key;
                    entry.1 = value;
                }
            } else {
                procedure_key_values.insert(curr_key.procedure_id, (curr_key, value));
            }
        }

        for (procedure_id, (parsed_key, value)) in procedure_key_values {
            if !parsed_key.is_committed {
                let Some(message) = self.load_one_message(&parsed_key, &value) else {
                    // We don't abort the loading process and just ignore errors to ensure all remaining
                    // procedures are loaded.
                    continue;
                };
                messages.insert(procedure_id, message);
            }
        }

        Ok(messages)
    }

    fn load_one_message(&self, key: &ParsedKey, value: &[u8]) -> Option<ProcedureMessage> {
        serde_json::from_slice(value)
            .map_err(|e| {
                // `e` doesn't impl ErrorExt so we print it as normal error.
                logging::error!("Failed to parse value, key: {:?}, source: {}", key, e);
                e
            })
            .ok()
    }
}

/// Key to refer the procedure in the [ProcedureStore].
#[derive(Debug, PartialEq, Eq)]
struct ParsedKey {
    procedure_id: ProcedureId,
    step: u32,
    is_committed: bool,
}

impl fmt::Display for ParsedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{:010}.{}",
            self.procedure_id,
            self.step,
            if self.is_committed { "commit" } else { "step" }
        )
    }
}

impl ParsedKey {
    /// Try to parse the key from specific `input`.
    fn parse_str(input: &str) -> Option<ParsedKey> {
        let mut iter = input.rsplit('/');
        let name = iter.next()?;
        let id_str = iter.next()?;

        let procedure_id = ProcedureId::parse_str(id_str).ok()?;

        let mut parts = name.split('.');
        let step_str = parts.next()?;
        let suffix = parts.next()?;
        let is_committed = match suffix {
            "commit" => true,
            "step" => false,
            _ => return None,
        };
        let step = step_str.parse().ok()?;

        Some(ParsedKey {
            procedure_id,
            step,
            is_committed,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use object_store::services::fs::Builder;
    use object_store::ObjectStore;
    use tempdir::TempDir;

    use super::*;
    use crate::store::state_store::ObjectStateStore;
    use crate::{Context, LockKey, Procedure, Status};

    #[test]
    fn test_parsed_key() {
        let procedure_id = ProcedureId::random();
        let key = ParsedKey {
            procedure_id,
            step: 2,
            is_committed: false,
        };
        assert_eq!(format!("{procedure_id}/0000000002.step"), key.to_string());
        assert_eq!(key, ParsedKey::parse_str(&key.to_string()).unwrap());

        let key = ParsedKey {
            procedure_id,
            step: 2,
            is_committed: true,
        };
        assert_eq!(format!("{procedure_id}/0000000002.commit"), key.to_string());
        assert_eq!(key, ParsedKey::parse_str(&key.to_string()).unwrap());
    }

    #[test]
    fn test_parse_invalid_key() {
        assert!(ParsedKey::parse_str("").is_none());

        let procedure_id = ProcedureId::random();
        let input = format!("{procedure_id}");
        assert!(ParsedKey::parse_str(&input).is_none());

        let input = format!("{procedure_id}/");
        assert!(ParsedKey::parse_str(&input).is_none());

        let input = format!("{procedure_id}/0000000003");
        assert!(ParsedKey::parse_str(&input).is_none());

        let input = format!("{procedure_id}/0000000003.");
        assert!(ParsedKey::parse_str(&input).is_none());

        let input = format!("{procedure_id}/0000000003.other");
        assert!(ParsedKey::parse_str(&input).is_none());

        assert!(ParsedKey::parse_str("12345/0000000003.step").is_none());

        let input = format!("{procedure_id}-0000000003.commit");
        assert!(ParsedKey::parse_str(&input).is_none());
    }

    #[test]
    fn test_procedure_message() {
        let mut message = ProcedureMessage {
            type_name: "TestMessage".to_string(),
            data: "no parent id".to_string(),
            parent_id: None,
        };

        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"type_name":"TestMessage","data":"no parent id","parent_id":null}"#
        );

        let procedure_id = ProcedureId::parse_str("9f805a1f-05f7-490c-9f91-bd56e3cc54c1").unwrap();
        message.parent_id = Some(procedure_id);
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(
            json,
            r#"{"type_name":"TestMessage","data":"no parent id","parent_id":"9f805a1f-05f7-490c-9f91-bd56e3cc54c1"}"#
        );
    }

    struct MockProcedure {
        data: String,
    }

    impl MockProcedure {
        fn new(data: impl Into<String>) -> MockProcedure {
            MockProcedure { data: data.into() }
        }
    }

    #[async_trait]
    impl Procedure for MockProcedure {
        fn type_name(&self) -> &str {
            "MockProcedure"
        }

        async fn execute(&mut self, _ctx: &Context) -> Result<Status> {
            unimplemented!()
        }

        fn dump(&self) -> Result<String> {
            Ok(self.data.clone())
        }

        fn lock_key(&self) -> Option<LockKey> {
            None
        }
    }

    fn new_procedure_store(dir: &TempDir) -> ProcedureStore {
        let store_dir = dir.path().to_str().unwrap();
        let accessor = Builder::default().root(store_dir).build().unwrap();
        let object_store = ObjectStore::new(accessor);
        let state_store = ObjectStateStore::new(object_store);

        ProcedureStore(Arc::new(state_store))
    }

    #[tokio::test]
    async fn test_store_procedure() {
        let dir = TempDir::new("store_procedure").unwrap();
        let store = new_procedure_store(&dir);

        let procedure_id = ProcedureId::random();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("test store procedure"));

        store
            .store_procedure(procedure_id, 0, &procedure, None)
            .await
            .unwrap();

        let messages = store.load_messages().await.unwrap();
        assert_eq!(1, messages.len());
        let msg = messages.get(&procedure_id).unwrap();
        let expect = ProcedureMessage {
            type_name: "MockProcedure".to_string(),
            data: "test store procedure".to_string(),
            parent_id: None,
        };
        assert_eq!(expect, *msg);
    }

    #[tokio::test]
    async fn test_commit_procedure() {
        let dir = TempDir::new("store_procedure").unwrap();
        let store = new_procedure_store(&dir);

        let procedure_id = ProcedureId::random();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("test store procedure"));

        store
            .store_procedure(procedure_id, 0, &procedure, None)
            .await
            .unwrap();
        store.commit_procedure(procedure_id, 1).await.unwrap();

        let messages = store.load_messages().await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_load_messages() {
        let dir = TempDir::new("store_procedure").unwrap();
        let store = new_procedure_store(&dir);

        // store 3 steps
        let id0 = ProcedureId::random();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("id0-0"));
        store
            .store_procedure(id0, 0, &procedure, None)
            .await
            .unwrap();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("id0-1"));
        store
            .store_procedure(id0, 1, &procedure, None)
            .await
            .unwrap();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("id0-2"));
        store
            .store_procedure(id0, 2, &procedure, None)
            .await
            .unwrap();

        // store 2 steps and then commit
        let id1 = ProcedureId::random();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("id1-0"));
        store
            .store_procedure(id1, 0, &procedure, None)
            .await
            .unwrap();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("id1-1"));
        store
            .store_procedure(id1, 1, &procedure, None)
            .await
            .unwrap();
        store.commit_procedure(id1, 2).await.unwrap();

        // store 1 step
        let id2 = ProcedureId::random();
        let procedure: BoxedProcedure = Box::new(MockProcedure::new("id2-0"));
        store
            .store_procedure(id2, 0, &procedure, None)
            .await
            .unwrap();

        let messages = store.load_messages().await.unwrap();
        assert_eq!(2, messages.len());

        let msg = messages.get(&id0).unwrap();
        assert_eq!("id0-2", msg.data);
        let msg = messages.get(&id2).unwrap();
        assert_eq!("id2-0", msg.data);
    }
}
