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

//! Scripts manager
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use catalog::CatalogManagerRef;
use common_query::Output;
use common_telemetry::logging;
use query::QueryEngineRef;
use snafu::{OptionExt, ResultExt};

use crate::engine::{CompileContext, EvalContext, Script, ScriptEngine};
use crate::error::{CompilePythonSnafu, ExecutePythonSnafu, Result, ScriptNotFoundSnafu};
use crate::python::{PyEngine, PyScript};
use crate::table::ScriptsTable;

pub struct ScriptManager {
    compiled: RwLock<HashMap<String, Arc<PyScript>>>,
    py_engine: PyEngine,
    table: ScriptsTable,
}

impl ScriptManager {
    pub async fn new(
        catalog_manager: CatalogManagerRef,
        query_engine: QueryEngineRef,
    ) -> Result<Self> {
        Ok(Self {
            compiled: RwLock::new(HashMap::default()),
            py_engine: PyEngine::new(query_engine.clone()),
            table: ScriptsTable::new(catalog_manager, query_engine).await?,
        })
    }

    async fn compile(&self, name: &str, script: &str) -> Result<Arc<PyScript>> {
        let script = Arc::new(
            self.py_engine
                .compile(script, CompileContext::default())
                .await
                .context(CompilePythonSnafu { name })?,
        );

        let mut compiled = self.compiled.write().unwrap();
        compiled.insert(name.to_string(), script.clone());

        logging::info!("Compiled and cached script: {}", name);

        script.as_ref().register_udf();

        logging::info!("Script register as UDF: {}", name);

        Ok(script)
    }

    pub async fn insert_and_compile(&self, name: &str, script: &str) -> Result<Arc<PyScript>> {
        let compiled_script = self.compile(name, script).await?;
        self.table.insert(name, script).await?;
        Ok(compiled_script)
    }

    pub async fn execute(&self, name: &str) -> Result<Output> {
        let script = {
            let s = self.compiled.read().unwrap().get(name).cloned();

            if s.is_some() {
                s
            } else {
                self.try_find_script_and_compile(name).await?
            }
        };

        let script = script.context(ScriptNotFoundSnafu { name })?;

        script
            .execute(EvalContext::default())
            .await
            .context(ExecutePythonSnafu { name })
    }

    async fn try_find_script_and_compile(&self, name: &str) -> Result<Option<Arc<PyScript>>> {
        let script = self.table.find_script_by_name(name).await?;

        Ok(Some(self.compile(name, &script).await?))
    }
}

#[cfg(test)]
mod tests {
    use catalog::CatalogManager;
    use mito::config::EngineConfig as TableEngineConfig;
    use mito::table::test_util::new_test_object_store;
    use query::QueryEngineFactory;

    use super::*;
    type DefaultEngine = MitoEngine<EngineImpl<RaftEngineLogStore>>;
    use log_store::raft_engine::log_store::RaftEngineLogStore;
    use log_store::LogConfig;
    use mito::engine::MitoEngine;
    use storage::config::EngineConfig as StorageEngineConfig;
    use storage::EngineImpl;
    use tempdir::TempDir;

    #[tokio::test]
    async fn test_insert_find_compile_script() {
        let wal_dir = TempDir::new("test_insert_find_compile_script_wal").unwrap();
        let wal_dir_str = wal_dir.path().to_string_lossy();

        common_telemetry::init_default_ut_logging();
        let (_dir, object_store) = new_test_object_store("test_insert_find_compile_script").await;
        let log_config = LogConfig {
            log_file_dir: wal_dir_str.to_string(),
            ..Default::default()
        };

        let log_store = RaftEngineLogStore::try_new(log_config).await.unwrap();
        let mock_engine = Arc::new(DefaultEngine::new(
            TableEngineConfig::default(),
            EngineImpl::new(
                StorageEngineConfig::default(),
                Arc::new(log_store),
                object_store.clone(),
            ),
            object_store,
        ));

        let catalog_manager = Arc::new(
            catalog::local::LocalCatalogManager::try_new(mock_engine.clone())
                .await
                .unwrap(),
        );

        let factory = QueryEngineFactory::new(catalog_manager.clone());
        let query_engine = factory.query_engine();
        let mgr = ScriptManager::new(catalog_manager.clone(), query_engine)
            .await
            .unwrap();
        catalog_manager.start().await.unwrap();

        let name = "test";
        mgr.table
            .insert(
                name,
                r#"
@copr(sql='select number from numbers limit 10', args=['number'], returns=['n'])
def test(n):
    return n + 1;
"#,
            )
            .await
            .unwrap();

        {
            let cached = mgr.compiled.read().unwrap();
            assert!(cached.get(name).is_none());
        }

        // try to find and compile
        let script = mgr.try_find_script_and_compile(name).await.unwrap();
        assert!(script.is_some());

        {
            let cached = mgr.compiled.read().unwrap();
            assert!(cached.get(name).is_some());
        }
    }
}
