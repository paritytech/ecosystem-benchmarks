use clap::Parser;
use codec::Decode;
use futures::{stream::FuturesUnordered, FutureExt, StreamExt, TryStreamExt};
use log::*;
use sender_lib::{connect, sign_balance_transfers};
use core::time;
use std::{
	collections::HashMap,
	error::Error,
	sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}, time::Instant,
};
// use subxt::{ext::sp_core::Pair, utils::AccountId32, OnlineClient, PolkadotConfig};

use subxt::{
	backend::legacy::rpc_methods::Block, blocks::BlockRef, config::polkadot::PolkadotExtrinsicParamsBuilder as Params, dynamic::Value, ext::sp_core::{sr25519::Pair as SrPair, Pair}, tx::{PairSigner, SubmittableExtrinsic, TransactionInvalid, ValidationResult}, OnlineClient, PolkadotConfig
};
use tokio::{sync::{Mutex, RwLock, Semaphore}, time::timeout};

const SENDER_SEED: &str = "//Sender";
const RECEIVER_SEED: &str = "//Receiver";

/// Util program to send transactions
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// Node URL. Can be either a collator, or relaychain node based on whether you want to measure parachain TPS, or relaychain TPS.
	#[arg(long)]
	node_url: String,

	/// Total number of senders
	#[arg(long)]
	total_senders: Option<usize>,

	/// Chunk size for sending the extrinsics.
	#[arg(long, default_value_t = 50)]
	chunk_size: usize,

	/// Total number of pre-funded accounts (on funded-accounts.json).
	#[arg(long)]
	tps: usize,
}

// FIXME: This assumes that all the chains supported by sTPS use this `AccountInfo` type. Currently,
// that holds. However, to benchmark a chain with another `AccountInfo` structure, a mechanism to
// adjust this type info should be provided.
type AccountInfo = frame_system::AccountInfo<u32, pallet_balances::AccountData<u128>>;

#[derive(Debug)]
enum AccountError {
	Subxt(subxt::Error),
	Codec,
}

impl std::fmt::Display for AccountError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			AccountError::Subxt(e) => write!(f, "Subxt error: {}", e.to_string()),
			AccountError::Codec => write!(f, "SCALE codec error"),
		}
	}
}

impl Error for AccountError {}

/// Check account nonce and free balance
// async fn check_account(
// 	api: OnlineClient<PolkadotConfig>,
// 	account: AccountId32,
// 	ext_deposit: u128,
// ) -> Result<(), AccountError> {
// 	let account_state_storage_addr = subxt::dynamic::storage("System", "Account", vec![account]);
// 	let finalized_head_hash = api
// 		.backend()
// 		.latest_finalized_block_ref()
// 		.await
// 		.map_err(AccountError::Subxt)?
// 		.hash();
// 	let account_state_encoded = api
// 		.storage()
// 		.at(finalized_head_hash)
// 		.fetch(&account_state_storage_addr)
// 		.await
// 		.map_err(AccountError::Subxt)?
// 		.expect("Existential deposit is set")
// 		.into_encoded();
// 	let account_state: AccountInfo =
// 		Decode::decode(&mut &account_state_encoded[..]).map_err(|_| AccountError::Codec)?;

// 	if account_state.nonce != 0 {
// 		panic!("Account has non-zero nonce");
// 	}

// 	// Reserve 10% for fees
// 	if (account_state.data.free as f64) < ext_deposit as f64 * 1.1 {
// 		panic!("Account has insufficient funds");
// 	}
// 	Ok(())
// }
use jsonrpsee_client_transport::ws::WsTransportClientBuilder;
use jsonrpsee_core::client::{async_client::PingConfig, Client};
use std::sync::Arc;
use subxt::backend::legacy::LegacyBackend;
use subxt::backend::unstable::UnstableBackend;

use tokio::time::Duration;

async fn get_account_nonce<C: subxt::Config>(api: &OnlineClient<C>, block: BlockRef<C::Hash>, account: &SrPair) -> u64 {
	let pubkey = account.public();
	let account_state_storage_addr = subxt::dynamic::storage(
		"System",
		"Account",
		vec![subxt::dynamic::Value::from_bytes(pubkey)],
	);

	let account_state_enc = api
		.storage()
		.at(block)
		.fetch(&account_state_storage_addr)
		.await
		.expect("Account status fetched")
		.expect("Nonce is set")
		.into_encoded();

	let account_state: AccountInfo =
		Decode::decode(&mut &account_state_enc[..]).expect("Account state decodes successfuly");
	account_state.nonce.into()
}


fn main() -> Result<(), Box<dyn Error>> {
	env_logger::init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let args = Args::parse();

	// Assume number of senders equal to TPS if not specified.
	let n_tx_sender = args.total_senders.unwrap_or(args.tps);
	let worker_sleep = (1_000f64 * (n_tx_sender as f64 / args.tps as f64)) as u64;

	log::info!("worker_sleep = {}",worker_sleep);

	let sender_accounts = funder_lib::derive_accounts(n_tx_sender, SENDER_SEED.to_owned());
	let receiver_accounts = funder_lib::derive_accounts(n_tx_sender, RECEIVER_SEED.to_owned());
	
	async fn create_api(node_url: String) -> OnlineClient<PolkadotConfig> {
		let node_url = url::Url::parse(&node_url).unwrap();
		let (node_sender, node_receiver) =
			WsTransportClientBuilder::default().build(node_url.clone()).await.unwrap();
		let client = Client::builder()
			.request_timeout(Duration::from_secs(10))
			.max_buffer_capacity_per_subscription(16 * 1024 * 1024)
			.enable_ws_ping(PingConfig::new().ping_interval(Duration::from_secs(10)))
			.set_tcp_no_delay(true)
			.max_concurrent_requests( 1024 * 10)
			.build_with_tokio(node_sender, node_receiver);
		let backend = Arc::new(LegacyBackend::builder().build(client));
		OnlineClient::from_backend(backend).await.unwrap()
	}

	loop {
		tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.unwrap()
			.block_on(async {
				let node_url = args.node_url.clone();
				let api = create_api(node_url.clone()).await;

				// Subscribe to best block stream
				let mut best_block_stream  = api.blocks().subscribe_best().await.expect("Subscribe to best block failed");
				let best_block = Arc::new(RwLock::new((best_block_stream.next().await.unwrap().unwrap(), Instant::now())));

				log::info!("Current best block: {}", best_block.read().await.0.number() );

				let sender_signers = sender_accounts
					.iter()
					.cloned()
					.map(PairSigner::<PolkadotConfig, SrPair>::new)
					.collect::<Vec<_>>();

				let now = std::time::Instant::now();
				let block_ref: BlockRef<subxt::utils::H256> = BlockRef::from_hash(best_block.read().await.0.hash());

				info!("Starting senders");

				// Overall metrics that we use to throttle
				// Transactions sent since last block
				let sent = Arc::new(AtomicU64::default());
				// Number of in block transactions.
				let in_block = Arc::new(AtomicU64::default());

				let mut handles = Vec::new();
				let mut timestamp = Duration::from_micros(0);
				let mut block_time = Duration::from_micros(0);

				loop {
					sent.store(0, Ordering::SeqCst);
					in_block.store(0, Ordering::SeqCst);
					
					// Spawn 1 task per sender.
					for i in 0..n_tx_sender {
						let in_block = in_block.clone();
						let sent = sent.clone();

						let node_url = node_url.clone();

						let sender = sender_accounts[i].clone();
						let signer = sender_signers[i].clone();
						

						let best_block = best_block.clone();
						let sent = sent.clone();
						let in_block = in_block.clone();

						// Use one API instance per 1/10 workers.
						let api = if i % 3000 == 0  {
							create_api(node_url.clone()).await
						} else {
							api.clone()
						};

						let receiver = receiver_accounts[i].clone();

						// TODO: Fix future transaction problem ....
						let task = async move {
							let mut sleep_time_ms = 0u64;	
							let block_ref: BlockRef<subxt::utils::H256> = BlockRef::from_hash(best_block.read().await.0.hash());
							let mut nonce = get_account_nonce(&api, block_ref.clone(), &sender).await;

							loop {
								if sent.load(Ordering::SeqCst) > in_block.load(Ordering::SeqCst) + 12000 { // TODO: rpc pool size
									// Wait 10ms and check again.
									tokio::time::sleep(std::time::Duration::from_millis(10)).await;
									// Substract above sleep from TPS delay.
									sleep_time_ms = sleep_time_ms.saturating_sub(10);
									continue
								}

								// Target a rate of 1TPS per worker, so we wait.
								tokio::time::sleep(std::time::Duration::from_millis(sleep_time_ms)).await;
								let now = Instant::now();
								log::debug!("Sender {} using nonce {}", i, nonce);

								let tx_payload = 
									subxt::dynamic::tx(
										"Balances",
										"transfer_keep_alive",
										vec![
											Value::unnamed_variant("Id", [Value::from_bytes(receiver.public())]),
											Value::u128(1u32.into()),
										],
									);
								log::debug!("Sender {} using nonce {}", i, nonce);
								let tx_params = Params::new().nonce(nonce as u64).build();

								let tx = api
									.tx()
									.create_signed_offline(&tx_payload, &signer, tx_params)
									.unwrap();

								let mut watch = match tx.submit_and_watch().await {
									Ok(watch) => watch,
									Err(err) => {
										log::debug!("{:?}", err);
										let block_ref: BlockRef<subxt::utils::H256> = BlockRef::from_hash(best_block.read().await.0.hash());
										nonce = get_account_nonce(&api, block_ref, &sender).await;
										
										// at most 1 second
										sleep_time_ms = worker_sleep.saturating_sub(now.elapsed().as_millis() as u64);
										continue
									}
								};

								// log::debug!("Watching the tx");
								// // Wait up to 1s.
								// while let Some(a) = watch.next().await {
								// 	// Default value for retrying a transaction if failed.
								// 	sleep_time_ms = 100;
								// 	match a {
								// 		Ok(st) => match st {
								// 			subxt::tx::TxStatus::Validated => {
								// 				log::debug!("VALIDATED");
								// 				sent.fetch_add(1, Ordering::SeqCst);

								// 				// Determine how much left to sleep
								// 				sleep_time_ms = 1_000u64.saturating_sub(now.elapsed().as_millis() as u64);
								// 				nonce += 1;
								// 				break;
								// 			},
								// 			subxt::tx::TxStatus::Broadcasted { num_peers } =>
								// 				log::debug!("BROADCASTED TO {num_peers}"),
								// 			subxt::tx::TxStatus::NoLongerInBestBlock => {
								// 				log::debug!("NO LONGER IN BEST BLOCK");
								// 			},
								// 			subxt::tx::TxStatus::InBestBlock(_) => {
								// 				log::debug!("IN BEST BLOCK");
												
								// 			},
								// 			subxt::tx::TxStatus::InFinalizedBlock(_) =>
								// 				log::debug!("IN FINALIZED BLOCK"),
								// 			subxt::tx::TxStatus::Error { message } =>
								// 				log::debug!("ERROR: {message}"),
								// 			subxt::tx::TxStatus::Invalid { message } |
								// 			subxt::tx::TxStatus::Dropped { message } => {
								// 				log::debug!("INVALID/DROPPED: {message}");
								// 				let block_ref: BlockRef<subxt::utils::H256> = BlockRef::from_hash(best_block.read().await.0.hash());
								// 				nonce = get_account_nonce(&api, block_ref, &sender).await;
								// 				break;
								// 			},
								// 		},
								// 		Err(e) => {
								// 			warn!("Error status {:?}", e);
								// 		},
								// 	}
								// }

								sent.fetch_add(1, Ordering::SeqCst);
								// Determine how much left to sleep, we need to retry in 1000ms (backoff)
								sleep_time_ms = worker_sleep.saturating_sub(now.elapsed().as_millis() as u64);
								nonce += 1;
							}
							
						};
						handles.push(tokio::spawn(task));
					}

					log::info!("All senders started");

					let sent_a = sent.clone();
					let in_block_a = in_block.clone();

					// Tx pool drops transactions, or re-orgs happen.
					// This ensures we don't stop generating transactions because 
					// number of sent transactions is way higher then what we saw in blocks.
					// if round % 20 == 0 {
					// 	log::info!("New round begins");
					// 	sent.store(0, Ordering::SeqCst);
					// 	in_block.store(0, Ordering::SeqCst);
					// }
					for round in 0..10 {
						if let Ok(Some(new_best_block)) = best_block_stream.try_next().await {
							*best_block.write().await = (new_best_block, Instant::now());
						} else {
							log::error!("Best block subscription lost, trying to reconnect ... ");
							
							loop {
								match api.blocks().subscribe_best().await {
									Ok(fresh_best_block_stream) => {
										best_block_stream = fresh_best_block_stream;
										log::info!("Reconnected.");
										break;
									}
									Err(e) => {
										log::error!("Reconnect failed: {:?} ", e);
										tokio::time::sleep(std::time::Duration::from_millis(500)).await;
									}
								}
							}
							
						}

						let best_block = &best_block.read().await.0;
						
						let Ok(extrinsics) = best_block.extrinsics().await else {
							// Most likely, need to reconnect to RPC.
							continue
						};

						let mut txcount = 0;

						for ex in extrinsics.iter() {
							let Ok(ex) = ex else {
								continue
							};

							match (ex.pallet_name().expect("pallet name"), ex.variant_name().expect("variant name")) {
								("Timestamp", "set") => {
									let new_timestamp = Duration::from_millis(codec::Compact::<u64>::decode(&mut &ex.field_bytes()[..]).expect("timestamp decodes").0);
									block_time =  new_timestamp - timestamp;
									timestamp = new_timestamp;
								},
								("Balances", "transfer_keep_alive") | ("Nfts", "transfer") => {
									txcount += 1;
								},
								_ => (),
							}
						}

						in_block.fetch_add(txcount , Ordering::SeqCst);

						log::info!("TPS: {} \t| Sent/Inblock: {}/{} \t| Best: {} | tx_count = {} | block_time = {:?}", txcount * 1000 / block_time.as_millis() as u64, sent.load(Ordering::SeqCst),  in_block.load(Ordering::SeqCst), best_block.number(), txcount, block_time);
					}

					// Restarting
					for handle in handles.iter() {
						handle.abort();
					}
					log::info!("New round begins");
				}
				
			});
	}
}

