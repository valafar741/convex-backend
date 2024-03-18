use std::sync::Arc;

use common::{
    persistence::Persistence,
    runtime::{
        Runtime,
        SpawnHandle,
    },
    testing::TestPersistence,
};
use events::usage::NoOpUsageEventLogger;
use search::{
    searcher::SearcherStub,
    Searcher,
};
use storage::{
    LocalDirStorage,
    Storage,
};

use crate::{
    Database,
    ShutdownSignal,
    Transaction,
    VirtualSystemMapping,
};

pub struct DbFixtures<RT: Runtime> {
    pub tp: Box<dyn Persistence>,
    pub db: Database<RT>,
    pub searcher: Arc<dyn Searcher>,
    pub search_storage: Arc<dyn Storage>,
}

#[derive(Default)]
pub struct DbFixturesArgs {
    pub tp: Option<Box<dyn Persistence>>,
    pub searcher: Option<Arc<dyn Searcher>>,
    pub search_storage: Option<Arc<dyn Storage>>,
    pub virtual_system_mapping: VirtualSystemMapping,
}

impl<RT: Runtime> DbFixtures<RT> {
    pub async fn new(rt: &RT) -> anyhow::Result<Self> {
        Self::new_with_args(rt, DbFixturesArgs::default()).await
    }

    pub async fn new_with_args(
        rt: &RT,
        DbFixturesArgs {
            tp,
            searcher,
            search_storage,
            virtual_system_mapping,
        }: DbFixturesArgs,
    ) -> anyhow::Result<Self> {
        let tp = tp.unwrap_or_else(|| Box::new(TestPersistence::new()));
        let searcher = searcher.unwrap_or_else(|| Arc::new(SearcherStub {}));
        let search_storage = match search_storage {
            Some(ss) => ss,
            None => Arc::new(LocalDirStorage::new(rt.clone())?),
        };
        let db = Database::load(
            tp.clone(),
            rt.clone(),
            searcher.clone(),
            ShutdownSignal::panic(),
            virtual_system_mapping,
            Arc::new(NoOpUsageEventLogger),
        )
        .await?;
        db.set_search_storage(search_storage.clone());
        db.start_vector_bootstrap().into_join_future().await?;
        Ok(Self {
            tp,
            db,
            searcher,
            search_storage,
        })
    }
}

pub async fn new_test_database<RT: Runtime>(rt: RT) -> Database<RT> {
    DbFixtures::new(&rt).await.unwrap().db
}

pub async fn new_tx<RT: Runtime>(rt: RT) -> anyhow::Result<Transaction<RT>> {
    new_test_database(rt).await.begin_system().await
}
