mod backfill_state;
mod index_config;
mod index_snapshot;
mod index_state;

pub use self::{
    backfill_state::{
        TextBackfillCursor,
        TextIndexBackfillState,
    },
    index_config::{
        DeveloperSearchIndexConfig,
        SerializedDeveloperSearchIndexConfig,
    },
    index_snapshot::{
        FragmentedTextSegment,
        TextIndexSnapshot,
        TextIndexSnapshotData,
        TextSnapshotVersion,
    },
    index_state::{
        SerializedTextIndexState,
        TextIndexState,
    },
};

#[cfg(test)]
mod tests {
    use cmd_util::env::env_config;
    use proptest::prelude::*;
    use sync_types::testing::assert_roundtrips;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64 * env_config("CONVEX_PROPTEST_MULTIPLIER", 1), failure_persistence: None, .. ProptestConfig::default() })]

        #[test]
        fn test_developer_search_index_config_roundtrips(v in any::<DeveloperSearchIndexConfig>()) {
                assert_roundtrips::<
                DeveloperSearchIndexConfig,
                pb::searchlight::SearchIndexConfig
            >(v);
        }
    }
}
