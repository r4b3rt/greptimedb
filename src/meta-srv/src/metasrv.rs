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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use api::v1::meta::Peer;
use common_telemetry::{info, warn};
use serde::{Deserialize, Serialize};

use crate::election::Election;
use crate::handler::{
    CheckLeaderHandler, CollectStatsHandler, HeartbeatHandlerGroup, KeepLeaseHandler,
    OnLeaderStartHandler, PersistStatsHandler, ResponseHeaderHandler,
};
use crate::selector::lease_based::LeaseBasedSelector;
use crate::selector::{Selector, SelectorType};
use crate::sequence::{Sequence, SequenceRef};
use crate::service::store::kv::{KvStoreRef, ResetableKvStoreRef};
use crate::service::store::memory::MemStore;

pub const TABLE_ID_SEQ: &str = "table_id";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetaSrvOptions {
    pub bind_addr: String,
    pub server_addr: String,
    pub store_addr: String,
    pub datanode_lease_secs: i64,
    pub selector: SelectorType,
    pub use_memory_store: bool,
}

impl Default for MetaSrvOptions {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:3002".to_string(),
            server_addr: "127.0.0.1:3002".to_string(),
            store_addr: "127.0.0.1:2379".to_string(),
            datanode_lease_secs: 15,
            selector: SelectorType::default(),
            use_memory_store: false,
        }
    }
}

#[derive(Clone)]
pub struct Context {
    pub datanode_lease_secs: i64,
    pub server_addr: String,
    pub in_memory: ResetableKvStoreRef,
    pub kv_store: KvStoreRef,
    pub election: Option<ElectionRef>,
    pub skip_all: Arc<AtomicBool>,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
}

impl Context {
    pub fn is_skip_all(&self) -> bool {
        self.skip_all.load(Ordering::Relaxed)
    }

    pub fn set_skip_all(&self) {
        self.skip_all.store(true, Ordering::Relaxed);
    }

    pub fn reset_in_memory(&self) {
        self.in_memory.reset();
    }
}

pub struct LeaderValue(pub String);

pub type SelectorRef = Arc<dyn Selector<Context = Context, Output = Vec<Peer>>>;
pub type ElectionRef = Arc<dyn Election<Leader = LeaderValue>>;

#[derive(Clone)]
pub struct MetaSrv {
    started: Arc<AtomicBool>,
    options: MetaSrvOptions,
    // It is only valid at the leader node and is used to temporarily
    // store some data that will not be persisted.
    in_memory: ResetableKvStoreRef,
    kv_store: KvStoreRef,
    table_id_sequence: SequenceRef,
    selector: SelectorRef,
    handler_group: HeartbeatHandlerGroup,
    election: Option<ElectionRef>,
}

impl MetaSrv {
    pub async fn new(
        options: MetaSrvOptions,
        kv_store: KvStoreRef,
        selector: Option<SelectorRef>,
        election: Option<ElectionRef>,
        handler_group: Option<HeartbeatHandlerGroup>,
    ) -> Self {
        let started = Arc::new(AtomicBool::new(false));
        let table_id_sequence = Arc::new(Sequence::new(TABLE_ID_SEQ, 1024, 10, kv_store.clone()));
        let selector = selector.unwrap_or_else(|| Arc::new(LeaseBasedSelector {}));
        let in_memory = Arc::new(MemStore::default());
        let handler_group = match handler_group {
            Some(hg) => hg,
            None => {
                let group = HeartbeatHandlerGroup::default();
                let keep_lease_handler = KeepLeaseHandler::new(kv_store.clone());
                group.add_handler(ResponseHeaderHandler::default()).await;
                // `KeepLeaseHandler` should preferably be in front of `CheckLeaderHandler`,
                // because even if the current meta-server node is no longer the leader it can
                // still help the datanode to keep lease.
                group.add_handler(keep_lease_handler).await;
                group.add_handler(CheckLeaderHandler::default()).await;
                group.add_handler(OnLeaderStartHandler::default()).await;
                group.add_handler(CollectStatsHandler::default()).await;
                group.add_handler(PersistStatsHandler::default()).await;
                group
            }
        };

        Self {
            started,
            options,
            in_memory,
            kv_store,
            table_id_sequence,
            selector,
            handler_group,
            election,
        }
    }

    pub async fn start(&self) {
        if self
            .started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            warn!("MetaSrv already started");
            return;
        }

        if let Some(election) = self.election() {
            let election = election.clone();
            let started = self.started.clone();
            common_runtime::spawn_bg(async move {
                while started.load(Ordering::Relaxed) {
                    let res = election.campaign().await;
                    if let Err(e) = res {
                        warn!("MetaSrv election error: {}", e);
                    }
                    info!("MetaSrv re-initiate election");
                }
                info!("MetaSrv stopped");
            });
        }

        info!("MetaSrv started");
    }

    pub fn shutdown(&self) {
        self.started.store(false, Ordering::Relaxed);
    }

    #[inline]
    pub fn options(&self) -> &MetaSrvOptions {
        &self.options
    }

    #[inline]
    pub fn in_memory(&self) -> ResetableKvStoreRef {
        self.in_memory.clone()
    }

    #[inline]
    pub fn kv_store(&self) -> KvStoreRef {
        self.kv_store.clone()
    }

    #[inline]
    pub fn table_id_sequence(&self) -> SequenceRef {
        self.table_id_sequence.clone()
    }

    #[inline]
    pub fn selector(&self) -> SelectorRef {
        self.selector.clone()
    }

    #[inline]
    pub fn handler_group(&self) -> HeartbeatHandlerGroup {
        self.handler_group.clone()
    }

    #[inline]
    pub fn election(&self) -> Option<ElectionRef> {
        self.election.clone()
    }

    #[inline]
    pub fn new_ctx(&self) -> Context {
        let datanode_lease_secs = self.options().datanode_lease_secs;
        let server_addr = self.options().server_addr.clone();
        let in_memory = self.in_memory();
        let kv_store = self.kv_store();
        let election = self.election();
        let skip_all = Arc::new(AtomicBool::new(false));
        Context {
            datanode_lease_secs,
            server_addr,
            in_memory,
            kv_store,
            election,
            skip_all,
            catalog: None,
            schema: None,
            table: None,
        }
    }
}
