use crate::{
    conductor::{dna_store::MockDnaStore, ConductorHandle},
    core::{
        state::{validation_db::ValidationLimboStatus, workspace::Workspace},
        workflow::incoming_dht_ops_workflow::IncomingDhtOpsWorkspace,
    },
    test_utils::{host_fn_api::*, setup_app},
};
use ::fixt::prelude::*;
use fallible_iterator::FallibleIterator;
use hdk3::prelude::LinkTag;
use holo_hash::{AnyDhtHash, EntryHash, HeaderHash};
use holochain_serialized_bytes::{SerializedBytes, UnsafeBytes};
use holochain_state::prelude::ReadManager;
use holochain_types::{
    app::InstalledCell, cell::CellId, dht_op::DhtOpLight, dna::DnaDef, dna::DnaFile, fixt::*,
    test_utils::fake_agent_pubkey_1, test_utils::fake_agent_pubkey_2, validate::ValidationStatus,
    Entry,
};
use holochain_wasm_test_utils::TestWasm;
use std::{
    convert::{TryFrom, TryInto},
    time::Duration,
};
use tracing::*;

#[tokio::test(threaded_scheduler)]
async fn sys_validation_workflow_test() {
    observability::test_run().ok();

    let dna_file = DnaFile::new(
        DnaDef {
            name: "sys_validation_workflow_test".to_string(),
            uuid: "ba1d046d-ce29-4778-914b-47e6010d2faf".to_string(),
            properties: SerializedBytes::try_from(()).unwrap(),
            zomes: vec![TestWasm::CommitEntry.into()].into(),
        },
        vec![TestWasm::CommitEntry.into()],
    )
    .await
    .unwrap();

    let alice_agent_id = fake_agent_pubkey_1();
    let alice_cell_id = CellId::new(dna_file.dna_hash().to_owned(), alice_agent_id.clone());
    let alice_installed_cell = InstalledCell::new(alice_cell_id.clone(), "alice_handle".into());

    let bob_agent_id = fake_agent_pubkey_2();
    let bob_cell_id = CellId::new(dna_file.dna_hash().to_owned(), bob_agent_id.clone());
    let bob_installed_cell = InstalledCell::new(bob_cell_id.clone(), "bob_handle".into());

    let mut dna_store = MockDnaStore::new();

    dna_store.expect_get().return_const(Some(dna_file.clone()));
    dna_store.expect_add_dnas::<Vec<_>>().return_const(());
    dna_store.expect_add_entry_defs::<Vec<_>>().return_const(());
    dna_store.expect_get_entry_def().return_const(None);

    let (_tmpdir, _app_api, handle) = setup_app(
        vec![(
            "test_app",
            vec![(alice_installed_cell, None), (bob_installed_cell, None)],
        )],
        dna_store,
    )
    .await;

    run_test(alice_cell_id, bob_cell_id, handle.clone(), dna_file).await;

    let shutdown = handle.take_shutdown_handle().await.unwrap();
    handle.shutdown().await;
    shutdown.await.unwrap();
}

async fn run_test(
    alice_cell_id: CellId,
    bob_cell_id: CellId,
    handle: ConductorHandle,
    dna_file: DnaFile,
) {
    bob_links_in_a_legit_way(&bob_cell_id, &handle, &dna_file).await;

    // Some time for ops to reach alice and run through validation
    tokio::time::delay_for(Duration::from_millis(500)).await;

    {
        let alice_env = handle.get_cell_env(&alice_cell_id).await.unwrap();
        let env_ref = alice_env.guard().await;
        let dbs = alice_env.dbs().await;
        let reader = env_ref.reader().unwrap();
        let workspace = IncomingDhtOpsWorkspace::new(&reader, &dbs).unwrap();
        // Validation should be empty
        assert_eq!(
            workspace.validation_limbo.iter().unwrap().count().unwrap(),
            0
        );
        // Integration should have 9 ops in it
        // Plus another 14 for genesis + init
        assert_eq!(
            workspace
                .integrated_dht_ops
                .iter()
                .unwrap()
                // Every op should be valid
                .inspect(|(_, i)| {
                    let s = debug_span!("inspect_ops");
                    let _g = s.enter();
                    debug!(?i.op);
                    assert_eq!(i.validation_status, ValidationStatus::Valid);
                    Ok(())
                })
                .count()
                .unwrap(),
            9 + 14
        );
    }

    let (big_entry_header, big_entry_hash, link_add_hash) =
        bob_makes_a_large_link(&bob_cell_id, &handle, &dna_file).await;
    info!("now");

    // Some time for ops to reach alice and run through validation
    tokio::time::delay_for(Duration::from_millis(500)).await;

    {
        let alice_env = handle.get_cell_env(&alice_cell_id).await.unwrap();
        let env_ref = alice_env.guard().await;
        let dbs = alice_env.dbs().await;
        let reader = env_ref.reader().unwrap();
        let workspace = IncomingDhtOpsWorkspace::new(&reader, &dbs).unwrap();
        // Validation should be empty
        assert_eq!(
            workspace
                .validation_limbo
                .iter()
                .unwrap()
                .inspect(|(_, i)| {
                    let s = debug_span!("inspect_ops");
                    let _g = s.enter();
                    debug!(?i.op);
                    assert_eq!(i.status, ValidationLimboStatus::Pending);
                    Ok(())
                })
                .count()
                .unwrap(),
            0
        );
        let big_entry_hash: AnyDhtHash = big_entry_hash.into();
        // Integration should have 12 ops in it
        // Plus the original 23
        assert_eq!(
            workspace
                .integrated_dht_ops
                .iter()
                .unwrap()
                // Every op should be valid except register updated by
                // Store entry for the update
                .inspect(|(_, i)| {
                    let s = debug_span!("inspect_ops");
                    let _g = s.enter();
                    debug!(?i.op);
                    match &i.op {
                        DhtOpLight::StoreEntry(hh, _, eh)
                            if eh == &big_entry_hash && hh == &big_entry_header =>
                        {
                            assert_eq!(i.validation_status, ValidationStatus::Rejected)
                        }
                        DhtOpLight::RegisterAddLink(hh, _) if hh == &link_add_hash => {
                            assert_eq!(i.validation_status, ValidationStatus::Rejected)
                        }
                        _ => assert_eq!(i.validation_status, ValidationStatus::Valid),
                    }
                    Ok(())
                })
                .count()
                .unwrap(),
            12 + 23
        );
    }

    dodgy_bob(&bob_cell_id, &handle, &dna_file).await;

    // Some time for ops to reach alice and run through validation
    tokio::time::delay_for(Duration::from_millis(500)).await;

    {
        let alice_env = handle.get_cell_env(&alice_cell_id).await.unwrap();
        let env_ref = alice_env.guard().await;
        let dbs = alice_env.dbs().await;
        let reader = env_ref.reader().unwrap();
        let workspace = IncomingDhtOpsWorkspace::new(&reader, &dbs).unwrap();
        // Validation should still contain bobs link pending because the target was missing
        assert_eq!(
            workspace
                .validation_limbo
                .iter()
                .unwrap()
                .inspect(|(_, i)| {
                    let s = debug_span!("inspect_ops");
                    let _g = s.enter();
                    debug!(?i.op);
                    assert_eq!(i.status, ValidationLimboStatus::Pending);
                    Ok(())
                })
                .count()
                .unwrap(),
            1
        );
        // Integration should have new 5 ops in it
        // Plus the original 35
        assert_eq!(
            workspace
                .integrated_dht_ops
                .iter()
                .unwrap()
                .count()
                .unwrap(),
            5 + 35
        );
    }
}

async fn bob_links_in_a_legit_way(
    bob_cell_id: &CellId,
    handle: &ConductorHandle,
    dna_file: &DnaFile,
) -> HeaderHash {
    let base = Post("Bananas are good for you".into());
    let target = Post("Potassium is radioactive".into());
    let base_entry_hash = EntryHash::with_data(&Entry::try_from(base.clone()).unwrap()).await;
    let target_entry_hash = EntryHash::with_data(&Entry::try_from(target.clone()).unwrap()).await;
    let link_tag = fixt!(LinkTag);
    let (bob_env, call_data) = CallData::create(bob_cell_id, handle, dna_file).await;
    let env_ref = bob_env.guard().await;
    let dbs = bob_env.dbs().await;
    // 3
    commit_entry(
        &env_ref,
        &dbs,
        call_data.clone(),
        base.clone().try_into().unwrap(),
        POST_ID,
    )
    .await;

    // 4
    commit_entry(
        &env_ref,
        &dbs,
        call_data.clone(),
        target.clone().try_into().unwrap(),
        POST_ID,
    )
    .await;

    // 5
    // Link the entries
    let link_add_address = link_entries(
        &env_ref,
        &dbs,
        call_data.clone(),
        base_entry_hash.clone(),
        target_entry_hash.clone(),
        link_tag.clone(),
    )
    .await;

    // Produce and publish these commits
    let mut triggers = handle.get_cell_triggers(&bob_cell_id).await.unwrap();
    triggers.produce_dht_ops.trigger();
    link_add_address
}

async fn bob_makes_a_large_link(
    bob_cell_id: &CellId,
    handle: &ConductorHandle,
    dna_file: &DnaFile,
) -> (HeaderHash, EntryHash, HeaderHash) {
    let bytes = (0..16_000_000).map(|_| 0u8).into_iter().collect::<Vec<_>>();
    let big_base = Entry::App(SerializedBytes::from(UnsafeBytes::from(bytes)));
    let big_base_entry_hash =
        EntryHash::with_data(&Entry::try_from(big_base.clone()).unwrap()).await;

    let base = Post("Small time base".into());
    let target = Post("Spam it big time".into());
    let base_entry_hash = EntryHash::with_data(&Entry::try_from(base.clone()).unwrap()).await;
    let target_entry_hash = EntryHash::with_data(&Entry::try_from(target.clone()).unwrap()).await;

    let bytes = (0..401).map(|_| 0u8).into_iter().collect::<Vec<_>>();
    let link_tag = LinkTag(bytes);

    let (bob_env, call_data) = CallData::create(bob_cell_id, handle, dna_file).await;
    let env_ref = bob_env.guard().await;
    let dbs = bob_env.dbs().await;

    // 6
    commit_entry(
        &env_ref,
        &dbs,
        call_data.clone(),
        base.clone().try_into().unwrap(),
        POST_ID,
    )
    .await;

    // 7
    commit_entry(
        &env_ref,
        &dbs,
        call_data.clone(),
        target.clone().try_into().unwrap(),
        POST_ID,
    )
    .await;

    // 8
    // Commit a large header
    let link_add_address = link_entries(
        &env_ref,
        &dbs,
        call_data.clone(),
        base_entry_hash.clone(),
        target_entry_hash.clone(),
        link_tag.clone(),
    )
    .await;

    // 9
    // Commit a huge entry
    let big_entry_header = commit_entry(
        &env_ref,
        &dbs,
        call_data.clone(),
        big_base.clone().try_into().unwrap(),
        POST_ID,
    )
    .await;

    // Produce and publish these commits
    let mut triggers = handle.get_cell_triggers(&bob_cell_id).await.unwrap();
    triggers.produce_dht_ops.trigger();
    (big_entry_header, big_base_entry_hash, link_add_address)
}

async fn dodgy_bob(bob_cell_id: &CellId, handle: &ConductorHandle, dna_file: &DnaFile) {
    let base = Post("Bob is the best and I'll link to proof so you can check".into());
    let target = Post("Dodgy proof Bob is the best".into());
    let base_entry_hash = EntryHash::with_data(&Entry::try_from(base.clone()).unwrap()).await;
    let target_entry_hash = EntryHash::with_data(&Entry::try_from(target.clone()).unwrap()).await;
    let link_tag = fixt!(LinkTag);
    let (bob_env, call_data) = CallData::create(bob_cell_id, handle, dna_file).await;
    let env_ref = bob_env.guard().await;
    let dbs = bob_env.dbs().await;

    // 11
    commit_entry(
        &env_ref,
        &dbs,
        call_data.clone(),
        base.clone().try_into().unwrap(),
        POST_ID,
    )
    .await;

    // Whoops forgot to commit that proof

    // Link the entries
    link_entries(
        &env_ref,
        &dbs,
        call_data.clone(),
        base_entry_hash.clone(),
        target_entry_hash.clone(),
        link_tag.clone(),
    )
    .await;

    // Produce and publish these commits
    let mut triggers = handle.get_cell_triggers(&bob_cell_id).await.unwrap();
    triggers.produce_dht_ops.trigger();
}