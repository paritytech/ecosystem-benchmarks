use codec::Decode;
use std::time::Duration;

use futures::{stream::FuturesUnordered, StreamExt};
use log::*;
use std::error::Error;
use subxt::{
	config::SubstrateExtrinsicParamsBuilder as Params,
	dynamic::Value,
	ext::sp_core::{sr25519::Pair as SrPair, Pair},
	tx::{PairSigner, SubmittableExtrinsic},
	OnlineClient, PolkadotConfig,
};

/// Maximal number of connection attempts.
const MAX_ATTEMPTS: usize = 10;
/// Delay period between failed connection attempts.
const RETRY_DELAY: Duration = Duration::from_secs(1);

pub async fn connect(url: &str) -> Result<OnlineClient<PolkadotConfig>, Box<dyn Error>> {
	for i in 0..MAX_ATTEMPTS {
		debug!("Attempt #{}: Connecting to {}", i, url);
		match OnlineClient::<PolkadotConfig>::from_url(url).await {
			Ok(client) => {
				debug!("Connection established to: {}", url);
				return Ok(client);
			},
			Err(err) => {
				warn!("API client {} error: {:?}", url, err);
				tokio::time::sleep(RETRY_DELAY).await;
			},
		};
	}

	let err = format!("Failed to connect to {} after {} attempts", url, MAX_ATTEMPTS);
	error!("{}", err);
	Err(err.into())
}

pub fn sign_txs(
	api: OnlineClient<PolkadotConfig>,
	txs: impl Iterator<Item = (SrPair, SrPair)>,
) -> Result<Vec<SubmittableExtrinsic<PolkadotConfig, OnlineClient<PolkadotConfig>>>, Box<dyn Error>>
{
	let ext_deposit_query = subxt::dynamic::constant("Balances", "ExistentialDeposit");
	let ext_deposit =
		u128::decode(&mut &api.constants().at(&ext_deposit_query)?.into_encoded()[..])?;

	let t = std::thread::available_parallelism().unwrap_or(1usize.try_into().unwrap()).get();

	let mut tn = (0..t).cycle();
	let mut tranges: Vec<_> = (0..t).map(|_| Vec::new()).collect();
	txs.for_each(|tx| tranges[tn.next().unwrap()].push(tx));
	let mut threads = Vec::new();

	tranges.into_iter().for_each(|chunk| {
		let api = api.clone();
		threads.push(std::thread::spawn(move || {
			chunk
				.into_iter()
				.map(move |(sender, receiver)| {
					let signer = PairSigner::new(sender);
					let tx_params = Params::new().nonce(0).build();
					let tx_call = subxt::dynamic::tx(
						"Balances",
						"transfer_keep_alive",
						vec![
							Value::unnamed_variant("Id", [Value::from_bytes(receiver.public())]),
							Value::u128(ext_deposit),
						],
					);
					api.tx().create_signed_offline(&tx_call, &signer, tx_params)
				})
				.collect::<Vec<_>>()
		}));
	});

	Ok(threads
		.into_iter()
		.map(|h| h.join().unwrap())
		.flatten()
		.collect::<Result<Vec<_>, _>>()?)
}

/// Here the signed extrinsics are submitted.
pub async fn submit_txs(
	txs: Vec<SubmittableExtrinsic<PolkadotConfig, OnlineClient<PolkadotConfig>>>,
) -> Result<(), Box<dyn Error>> {
	let futs = txs.iter().map(|tx| tx.submit_and_watch()).collect::<FuturesUnordered<_>>();
	let res = futs.collect::<Vec<_>>().await;
	let res: Result<Vec<_>, _> = res.into_iter().collect();
	let res = res.expect("All the transactions submitted successfully");
	let mut statuses = futures::stream::select_all(res);
	while let Some(a) = statuses.next().await {
		match a {
			Ok(st) => match st {
				subxt::tx::TxStatus::Validated => log::info!("VALIDATED"),
				subxt::tx::TxStatus::Broadcasted { num_peers } =>
					log::info!("BROADCASTED TO {num_peers}"),
				subxt::tx::TxStatus::NoLongerInBestBlock => log::warn!("NO LONGER IN BEST BLOCK"),
				subxt::tx::TxStatus::InBestBlock(_) => log::info!("IN BEST BLOCK"),
				subxt::tx::TxStatus::InFinalizedBlock(_) => log::info!("IN FINALIZED BLOCK"),
				subxt::tx::TxStatus::Error { message } => log::warn!("ERROR: {message}"),
				subxt::tx::TxStatus::Invalid { message } => log::warn!("INVALID: {message}"),
				subxt::tx::TxStatus::Dropped { message } => log::warn!("DROPPED: {message}"),
			},
			Err(e) => {
				warn!("Error status {:?}", e);
			},
		}
	}
	Ok(())
}
