//! Fixed test vectors for the mempool.

use std::sync::Arc;

use color_eyre::Report;
use tokio::time;
use tower::{ServiceBuilder, ServiceExt};

use zebra_chain::{block::Block, parameters::Network, serialization::ZcashDeserializeInto};
use zebra_consensus::transaction as tx;
use zebra_state::Config as StateConfig;
use zebra_test::mock_service::{MockService, PanicAssertion};

use crate::components::{
    mempool::{self, storage::tests::unmined_transactions_in_blocks, *},
    sync::RecentSyncLengths,
};

/// A [`MockService`] representing the network service.
type MockPeerSet = MockService<zn::Request, zn::Response, PanicAssertion>;

/// The unmocked Zebra state service's type.
type StateService = Buffer<BoxService<zs::Request, zs::Response, zs::BoxError>, zs::Request>;

/// A [`MockService`] representing the Zebra transaction verifier service.
type MockTxVerifier = MockService<tx::Request, tx::Response, PanicAssertion, TransactionError>;

#[tokio::test]
async fn mempool_service_disabled() -> Result<(), Report> {
    // Using the mainnet for now
    let network = Network::Mainnet;

    let (mut service, _peer_set, _state_service, _tx_verifier, mut recent_syncs) =
        setup(network).await;

    // get the genesis block transactions from the Zcash blockchain.
    let mut unmined_transactions = unmined_transactions_in_blocks(..=10, network);
    let genesis_transaction = unmined_transactions
        .next()
        .expect("Missing genesis transaction");
    let more_transactions = unmined_transactions;

    // Test if mempool is disabled (it should start disabled)
    assert!(!service.is_enabled());

    // Enable the mempool
    let _ = service.enable(&mut recent_syncs).await;

    assert!(service.is_enabled());

    // Insert the genesis block coinbase transaction into the mempool storage.
    service.storage().insert(genesis_transaction.clone())?;

    // Test if the mempool answers correctly (i.e. is enabled)
    let response = service
        .ready_and()
        .await
        .unwrap()
        .call(Request::TransactionIds)
        .await
        .unwrap();
    let _genesis_transaction_ids = match response {
        Response::TransactionIds(ids) => ids,
        _ => unreachable!("will never happen in this test"),
    };

    // Queue a transaction for download
    // Use the ID of the last transaction in the list
    let txid = more_transactions.last().unwrap().transaction.id;
    let response = service
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![txid.into()]))
        .await
        .unwrap();
    let queued_responses = match response {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    assert_eq!(queued_responses.len(), 1);
    assert!(queued_responses[0].is_ok());
    assert_eq!(service.tx_downloads().in_flight(), 1);

    // Disable the mempool
    let _ = service.disable(&mut recent_syncs).await;

    // Test if mempool is disabled again
    assert!(!service.is_enabled());

    // Test if the mempool returns no transactions when disabled
    let response = service
        .ready_and()
        .await
        .unwrap()
        .call(Request::TransactionIds)
        .await
        .unwrap();
    match response {
        Response::TransactionIds(ids) => {
            assert_eq!(
                ids.len(),
                0,
                "mempool should return no transactions when disabled"
            )
        }
        _ => unreachable!("will never happen in this test"),
    };

    // Test if the mempool returns to Queue requests correctly when disabled
    let response = service
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![txid.into()]))
        .await
        .unwrap();
    let queued_responses = match response {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    assert_eq!(queued_responses.len(), 1);
    assert_eq!(queued_responses[0], Err(MempoolError::Disabled));

    Ok(())
}

#[tokio::test]
async fn mempool_cancel_mined() -> Result<(), Report> {
    let block1: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_1_BYTES
        .zcash_deserialize_into()
        .unwrap();
    let block2: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_2_BYTES
        .zcash_deserialize_into()
        .unwrap();

    // Using the mainnet for now
    let network = Network::Mainnet;

    let (mut mempool, _peer_set, mut state_service, _tx_verifier, mut recent_syncs) =
        setup(network).await;

    time::pause();

    // Enable the mempool
    let _ = mempool.enable(&mut recent_syncs).await;
    assert!(mempool.is_enabled());

    // Push the genesis block to the state
    let genesis_block: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_GENESIS_BYTES
        .zcash_deserialize_into()
        .unwrap();
    state_service
        .ready_and()
        .await
        .unwrap()
        .call(zebra_state::Request::CommitFinalizedBlock(
            genesis_block.clone().into(),
        ))
        .await
        .unwrap();

    // Query the mempool to make it poll chain_tip_change
    mempool.dummy_call().await;

    // Push block 1 to the state
    state_service
        .ready_and()
        .await
        .unwrap()
        .call(zebra_state::Request::CommitFinalizedBlock(
            block1.clone().into(),
        ))
        .await
        .unwrap();

    // Query the mempool to make it poll chain_tip_change
    mempool.dummy_call().await;

    // Queue transaction from block 2 for download.
    // It can't be queued before because block 1 triggers a network upgrade,
    // which cancels all downloads.
    let txid = block2.transactions[0].unmined_id();
    let response = mempool
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![txid.into()]))
        .await
        .unwrap();
    let queued_responses = match response {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    assert_eq!(queued_responses.len(), 1);
    assert!(queued_responses[0].is_ok());
    assert_eq!(mempool.tx_downloads().in_flight(), 1);

    // Push block 2 to the state
    state_service
        .oneshot(zebra_state::Request::CommitFinalizedBlock(
            block2.clone().into(),
        ))
        .await
        .unwrap();

    // This is done twice because after the first query the cancellation
    // is picked up by select!, and after the second the mempool gets the
    // result and the download future is removed.
    for _ in 0..2 {
        // Query the mempool just to poll it and make it cancel the download.
        mempool.dummy_call().await;
        // Sleep to avoid starvation and make sure the cancellation is picked up.
        time::sleep(time::Duration::from_millis(100)).await;
    }

    // Check if download was cancelled.
    assert_eq!(mempool.tx_downloads().in_flight(), 0);

    Ok(())
}

#[tokio::test]
async fn mempool_cancel_downloads_after_network_upgrade() -> Result<(), Report> {
    let block1: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_1_BYTES
        .zcash_deserialize_into()
        .unwrap();
    let block2: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_2_BYTES
        .zcash_deserialize_into()
        .unwrap();

    // Using the mainnet for now
    let network = Network::Mainnet;

    let (mut mempool, _peer_set, mut state_service, _tx_verifier, mut recent_syncs) =
        setup(network).await;

    // Enable the mempool
    let _ = mempool.enable(&mut recent_syncs).await;
    assert!(mempool.is_enabled());

    // Push the genesis block to the state
    let genesis_block: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_GENESIS_BYTES
        .zcash_deserialize_into()
        .unwrap();
    state_service
        .ready_and()
        .await
        .unwrap()
        .call(zebra_state::Request::CommitFinalizedBlock(
            genesis_block.clone().into(),
        ))
        .await
        .unwrap();

    // Queue transaction from block 2 for download
    let txid = block2.transactions[0].unmined_id();
    let response = mempool
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![txid.into()]))
        .await
        .unwrap();
    let queued_responses = match response {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    assert_eq!(queued_responses.len(), 1);
    assert!(queued_responses[0].is_ok());
    assert_eq!(mempool.tx_downloads().in_flight(), 1);

    // Query the mempool to make it poll chain_tip_change
    mempool.dummy_call().await;

    // Push block 1 to the state. This is considered a network upgrade,
    // and thus must cancel all pending transaction downloads.
    state_service
        .ready_and()
        .await
        .unwrap()
        .call(zebra_state::Request::CommitFinalizedBlock(
            block1.clone().into(),
        ))
        .await
        .unwrap();

    // Query the mempool to make it poll chain_tip_change
    mempool.dummy_call().await;

    // Check if download was cancelled.
    assert_eq!(mempool.tx_downloads().in_flight(), 0);

    Ok(())
}

/// Check if a transaction that fails verification is rejected by the mempool.
#[tokio::test]
async fn mempool_failed_verification_is_rejected() -> Result<(), Report> {
    // Using the mainnet for now
    let network = Network::Mainnet;

    let (mut mempool, _peer_set, mut state_service, mut tx_verifier, mut recent_syncs) =
        setup(network).await;

    // Get transactions to use in the test
    let mut unmined_transactions = unmined_transactions_in_blocks(1..=2, network);
    let rejected_tx = unmined_transactions.next().unwrap().clone();

    time::pause();

    // Enable the mempool
    let _ = mempool.enable(&mut recent_syncs).await;

    // Push the genesis block to the state, since downloader needs a valid tip.
    let genesis_block: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_GENESIS_BYTES
        .zcash_deserialize_into()
        .unwrap();
    state_service
        .ready_and()
        .await
        .unwrap()
        .call(zebra_state::Request::CommitFinalizedBlock(
            genesis_block.clone().into(),
        ))
        .await
        .unwrap();

    // Queue first transaction for verification
    // (queue the transaction itself to avoid a download).
    let request = mempool
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![rejected_tx.transaction.clone().into()]));
    // Make the mock verifier return that the transaction is invalid.
    let verification = tx_verifier.expect_request_that(|_| true).map(|responder| {
        responder.respond(Err(TransactionError::BadBalance));
    });
    let (response, _) = futures::join!(request, verification);
    let queued_responses = match response.unwrap() {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    // Check that the request was enqueued successfully.
    assert_eq!(queued_responses.len(), 1);
    assert!(queued_responses[0].is_ok());

    for _ in 0..2 {
        // Query the mempool just to poll it and make get the downloader/verifier result.
        mempool.dummy_call().await;
        // Sleep to avoid starvation and make sure the verification failure is picked up.
        time::sleep(time::Duration::from_millis(100)).await;
    }

    // Try to queue the same transaction by its ID and check if it's correctly
    // rejected.
    let response = mempool
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![rejected_tx.transaction.id.into()]))
        .await
        .unwrap();
    let queued_responses = match response {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    assert_eq!(queued_responses.len(), 1);
    assert!(matches!(
        queued_responses[0],
        Err(MempoolError::StorageExactTip(
            ExactTipRejectionError::FailedVerification(_)
        ))
    ));

    Ok(())
}

/// Check if a transaction that fails download is _not_ rejected.
#[tokio::test]
async fn mempool_failed_download_is_not_rejected() -> Result<(), Report> {
    // Using the mainnet for now
    let network = Network::Mainnet;

    let (mut mempool, mut peer_set, mut state_service, _tx_verifier, mut recent_syncs) =
        setup(network).await;

    // Get transactions to use in the test
    let mut unmined_transactions = unmined_transactions_in_blocks(1..=2, network);
    let rejected_valid_tx = unmined_transactions.next().unwrap().clone();

    time::pause();

    // Enable the mempool
    let _ = mempool.enable(&mut recent_syncs).await;

    // Push the genesis block to the state, since downloader needs a valid tip.
    let genesis_block: Arc<Block> = zebra_test::vectors::BLOCK_MAINNET_GENESIS_BYTES
        .zcash_deserialize_into()
        .unwrap();
    state_service
        .ready_and()
        .await
        .unwrap()
        .call(zebra_state::Request::CommitFinalizedBlock(
            genesis_block.clone().into(),
        ))
        .await
        .unwrap();

    // Queue second transaction for download and verification.
    let request = mempool
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![rejected_valid_tx
            .transaction
            .id
            .into()]));
    // Make the mock peer set return that the download failed.
    let verification = peer_set
        .expect_request_that(|r| matches!(r, zn::Request::TransactionsById(_)))
        .map(|responder| {
            responder.respond(zn::Response::Transactions(vec![]));
        });
    let (response, _) = futures::join!(request, verification);
    let queued_responses = match response.unwrap() {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    // Check that the request was enqueued successfully.
    assert_eq!(queued_responses.len(), 1);
    assert!(queued_responses[0].is_ok());

    for _ in 0..2 {
        // Query the mempool just to poll it and make get the downloader/verifier result.
        mempool.dummy_call().await;
        // Sleep to avoid starvation and make sure the download failure is picked up.
        time::sleep(time::Duration::from_millis(100)).await;
    }

    // Try to queue the same transaction by its ID and check if it's not being
    // rejected.
    let response = mempool
        .ready_and()
        .await
        .unwrap()
        .call(Request::Queue(vec![rejected_valid_tx
            .transaction
            .id
            .into()]))
        .await
        .unwrap();
    let queued_responses = match response {
        Response::Queued(queue_responses) => queue_responses,
        _ => unreachable!("will never happen in this test"),
    };
    assert_eq!(queued_responses.len(), 1);
    assert!(queued_responses[0].is_ok());

    Ok(())
}

/// Create a new [`Mempool`] instance using mocked services.
async fn setup(
    network: Network,
) -> (
    Mempool,
    MockPeerSet,
    StateService,
    MockTxVerifier,
    RecentSyncLengths,
) {
    let peer_set = MockService::build().for_unit_tests();

    let state_config = StateConfig::ephemeral();
    let (state, latest_chain_tip, chain_tip_change) = zebra_state::init(state_config, network);
    let state_service = ServiceBuilder::new().buffer(1).service(state);

    let tx_verifier = MockService::build().for_unit_tests();

    let (sync_status, recent_syncs) = SyncStatus::new();

    let (mempool, _mempool_transaction_receiver) = Mempool::new(
        &mempool::Config {
            tx_cost_limit: u64::MAX,
            ..Default::default()
        },
        Buffer::new(BoxService::new(peer_set.clone()), 1),
        state_service.clone(),
        Buffer::new(BoxService::new(tx_verifier.clone()), 1),
        sync_status,
        latest_chain_tip,
        chain_tip_change,
    );

    (mempool, peer_set, state_service, tx_verifier, recent_syncs)
}
