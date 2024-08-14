#[macro_use]
extern crate clap;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{sleep, spawn};
use std::time::Duration;

use clap::App;
use grin_core::global;
use grin_core::global::ChainTypes;
use grin_util::{StopState, ZeroingString};
use rand::{Rng, thread_rng};
use rpassword;
use tor_rtcompat::PreferredRuntime;

use grin_onion::crypto;
use grin_onion::crypto::dalek::DalekPublicKey;
use mwixnet::config::{self, ServerConfig};
use mwixnet::mix_client::{MixClient, MixClientImpl};
use mwixnet::node::GrinNode;
use mwixnet::node::HttpGrinNode;
use mwixnet::servers;
use mwixnet::store::StoreError;
use mwixnet::store::SwapStore;
use mwixnet::tor;
use mwixnet::wallet::HttpWallet;

const DEFAULT_INTERVAL: u32 = 12 * 60 * 60;

fn main() -> Result<(), Box<dyn std::error::Error>> {
	real_main()?;
	std::process::exit(0);
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
	let yml = load_yaml!("mwixnet.yml");
	let args = App::from_yaml(yml).get_matches();
	let chain_type = if args.is_present("testnet") {
		ChainTypes::Testnet
	} else {
		ChainTypes::Mainnet
	};
	global::set_local_chain_type(chain_type);

	let config_path = match args.value_of("config_file") {
		Some(path) => PathBuf::from(path),
		None => {
			let mut grin_path = config::get_grin_path(&chain_type);
			grin_path.push("mwixnet-config.toml");
			grin_path
		}
	};

	let round_time = args
		.value_of("round_time")
		.map(|t| t.parse::<u32>().unwrap());
	let bind_addr = args.value_of("bind_addr");
	let grin_node_url = args.value_of("grin_node_url");
	let grin_node_secret_path = args.value_of("grin_node_secret_path");
	let wallet_owner_url = args.value_of("wallet_owner_url");
	let wallet_owner_secret_path = args.value_of("wallet_owner_secret_path");
	let prev_server = args
		.value_of("prev_server")
		.map(|p| DalekPublicKey::from_hex(&p).unwrap());
	let next_server = args
		.value_of("next_server")
		.map(|p| DalekPublicKey::from_hex(&p).unwrap());

	// Write a new config file if init-config command is supplied
	if let ("init-config", Some(_)) = args.subcommand() {
		if config_path.exists() {
			panic!(
				"Config file already exists at {}",
				config_path.to_string_lossy()
			);
		}

		let server_config = ServerConfig {
			key: crypto::secp::random_secret(),
			interval_s: round_time.unwrap_or(DEFAULT_INTERVAL),
			addr: bind_addr.unwrap_or("127.0.0.1:3000").parse()?,
			grin_node_url: match grin_node_url {
				Some(u) => u.parse()?,
				None => config::grin_node_url(&chain_type),
			},
			grin_node_secret_path: match grin_node_secret_path {
				Some(p) => Some(p.to_owned()),
				None => config::node_secret_path(&chain_type)
					.to_str()
					.map(|p| p.to_owned()),
			},
			wallet_owner_url: match wallet_owner_url {
				Some(u) => u.parse()?,
				None => config::wallet_owner_url(&chain_type),
			},
			wallet_owner_secret_path: match wallet_owner_secret_path {
				Some(p) => Some(p.to_owned()),
				None => config::wallet_owner_secret_path(&chain_type)
					.to_str()
					.map(|p| p.to_owned()),
			},
			prev_server,
			next_server,
		};

		let password = prompt_password_confirm();
		config::write_config(&config_path, &server_config, &password)?;
		println!(
			"Config file written to {:?}. Please back this file up in a safe place.",
			config_path
		);
		return Ok(());
	}

	let password = prompt_password();
	let mut server_config = config::load_config(&config_path, &password)?;

	// Write a new config file if init-config command is supplied
	if let ("pubkey", Some(_)) = args.subcommand() {
		if !config_path.exists() {
			panic!(
				"No public address configured (Config file not found). {}",
				config_path.to_string_lossy()
			);
		}
		let sub_args = args.subcommand_matches("pubkey").unwrap();
		let server_pubkey = server_config.server_pubkey();
		if sub_args.is_present("output_file") 
		{
			//output server pubkey to file
			let output_file = sub_args.value_of("output_file").unwrap();
			std::fs::write(output_file, format!("{}", server_pubkey.to_hex()))?;
			println!("Server pubkey written to file: {}", output_file);
		} else {
			println!("{}", server_pubkey.to_hex());
		}
		return Ok(())
	}

	// Override grin_node_url, if supplied
	if let Some(grin_node_url) = grin_node_url {
		server_config.grin_node_url = grin_node_url.parse()?;
	}

	// Override grin_node_secret_path, if supplied
	if let Some(grin_node_secret_path) = grin_node_secret_path {
		server_config.grin_node_secret_path = Some(grin_node_secret_path.to_owned());
	}

	// Override wallet_owner_url, if supplied
	if let Some(wallet_owner_url) = wallet_owner_url {
		server_config.wallet_owner_url = wallet_owner_url.parse()?;
	}

	// Override wallet_owner_secret_path, if supplied
	if let Some(wallet_owner_secret_path) = wallet_owner_secret_path {
		server_config.wallet_owner_secret_path = Some(wallet_owner_secret_path.to_owned());
	}

	// Override bind_addr, if supplied
	if let Some(bind_addr) = bind_addr {
		server_config.addr = bind_addr.parse()?;
	}

	// Override prev_server, if supplied
	if let Some(prev_server) = prev_server {
		server_config.prev_server = Some(prev_server);
	}

	// Override next_server, if supplied
	if let Some(next_server) = next_server {
		server_config.next_server = Some(next_server);
	}

	// Create GrinNode
	let node = HttpGrinNode::new(
		&server_config.grin_node_url,
		&server_config.node_api_secret(),
	);

	// Node API health check
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()?;

	let rt_handle = rt.handle().clone();

	if let Err(e) = rt_handle.block_on(node.async_get_chain_tip()) {
		eprintln!("Node communication failure. Is node listening?");
		return Err(e.into());
	};

	// Open wallet
	let wallet_pass = prompt_wallet_password(&args.value_of("wallet_pass"));
	let wallet = rt_handle.block_on(HttpWallet::async_open_wallet(
		&server_config.wallet_owner_url,
		&server_config.wallet_owner_api_secret(),
		&wallet_pass,
	));
	let wallet = match wallet {
		Ok(w) => w,
		Err(e) => {
			eprintln!("Wallet communication failure. Is wallet listening?");
			return Err(e.into());
		}
	};

	let tor_runtime = if let Ok(r) = PreferredRuntime::create() {
		r
	} else {
		return Err("No runtime found".into());
	};

	let tor_instance = rt_handle.block_on(tor::async_init_tor(
		tor_runtime,
		&config::get_grin_path(&chain_type).to_str().unwrap(),
		&server_config,
	))?;
	let tor_instance = Arc::new(grin_util::Mutex::new(tor_instance));
	let tor_clone = tor_instance.clone();

	let stop_state = Arc::new(StopState::new());
	let stop_state_clone = stop_state.clone();

	rt_handle.spawn(async move {
		futures::executor::block_on(build_signals_fut());
		tor_clone.lock().stop();
		stop_state_clone.stop();
	});

	let next_mixer: Option<Arc<dyn MixClient>> = server_config.next_server.clone().map(|pk| {
		let client: Arc<dyn MixClient> = Arc::new(MixClientImpl::new(
			server_config.clone(),
			tor_instance.clone(),
			pk.clone(),
		));
		client
	});

	if server_config.prev_server.is_some() {
		// Start the JSON-RPC HTTP 'mix' server
		println!(
			"Starting MIX server with public key {:?}",
			server_config.server_pubkey().to_hex()
		);

		let (_, http_server) = servers::mix_rpc::listen(
			&rt_handle,
			server_config,
			next_mixer,
			Arc::new(wallet),
			Arc::new(node),
		)?;

		let close_handle = http_server.close_handle();
		let round_handle = spawn(move || loop {
			if stop_state.is_stopped() {
				close_handle.close();
				break;
			}

			sleep(Duration::from_millis(100));
		});

		http_server.wait();
		round_handle.join().unwrap();
	} else {
		println!(
			"Starting SWAP server with public key {:?}",
			server_config.server_pubkey().to_hex()
		);

		// Open SwapStore
		let store = SwapStore::new(
			config::get_grin_path(&chain_type)
				.join("db")
				.to_str()
				.ok_or(StoreError::OpenError(grin_store::lmdb::Error::FileErr(
					"db_root path error".to_string(),
				)))?,
		)?;

		// Start the mwixnet JSON-RPC HTTP 'swap' server
		let (swap_server, http_server) = servers::swap_rpc::listen(
			rt.handle(),
			&server_config,
			next_mixer,
			Arc::new(wallet),
			Arc::new(node),
			store,
		)?;

		let close_handle = http_server.close_handle();
		let round_handle = spawn(move || {
			let mut rng = thread_rng();
			let mut secs = 0u32;
			let mut reorg_secs = 0u32;
			let mut reorg_window = rng.gen_range(900u32, 3600u32);
			let prev_tx = Arc::new(Mutex::new(None));
			let server = swap_server.clone();

			loop {
				if stop_state.is_stopped() {
					close_handle.close();
					break;
				}

				sleep(Duration::from_secs(1));
				secs = (secs + 1) % server_config.interval_s;
				reorg_secs = (reorg_secs + 1) % reorg_window;

				if secs == 0 {
					let prev_tx_clone = prev_tx.clone();
					let server_clone = server.clone();
					rt.spawn(async move {
						let result = server_clone.lock().await.execute_round().await;
						let mut prev_tx_lock = prev_tx_clone.lock().unwrap();
						*prev_tx_lock = match result {
							Ok(Some(tx)) => Some(tx),
							_ => None,
						};
					});
					reorg_secs = 0;
					reorg_window = rng.gen_range(900u32, 3600u32);
				} else if reorg_secs == 0 {
					let prev_tx_clone = prev_tx.clone();
					let server_clone = server.clone();
					rt.spawn(async move {
						let tx_option = {
							let prev_tx_lock = prev_tx_clone.lock().unwrap();
							prev_tx_lock.clone()
						}; // Lock is dropped here

						if let Some(tx) = tx_option {
							let result = server_clone.lock().await.check_reorg(&tx).await;
							let mut prev_tx_lock = prev_tx_clone.lock().unwrap();
							*prev_tx_lock = match result {
								Ok(Some(tx)) => Some(tx),
								_ => None,
							};
						}
					});
					reorg_window = rng.gen_range(900u32, 3600u32);
				}
			}
		});

		http_server.wait();
		round_handle.join().unwrap();
	}

	Ok(())
}

#[cfg(unix)]
async fn build_signals_fut() {
	use tokio::signal::unix::{signal, SignalKind};

	// Listen for SIGINT, SIGQUIT, and SIGTERM
	let mut terminate_signal =
		signal(SignalKind::terminate()).expect("failed to create terminate signal");
	let mut quit_signal = signal(SignalKind::quit()).expect("failed to create quit signal");
	let mut interrupt_signal =
		signal(SignalKind::interrupt()).expect("failed to create interrupt signal");

	futures::future::select_all(vec![
		Box::pin(terminate_signal.recv()),
		Box::pin(quit_signal.recv()),
		Box::pin(interrupt_signal.recv()),
	])
	.await;
}

#[cfg(not(unix))]
async fn build_signals_fut() {
	tokio::signal::ctrl_c()
		.await
		.expect("failed to install CTRL+C signal handler");
}

fn prompt_password() -> ZeroingString {
	ZeroingString::from(rpassword::prompt_password_stdout("Server password: ").unwrap())
}

fn prompt_password_confirm() -> ZeroingString {
	let mut first = "first".to_string();
	let mut second = "second".to_string();
	while first != second {
		first = rpassword::prompt_password_stdout("Server password: ").unwrap();
		second = rpassword::prompt_password_stdout("Confirm server password: ").unwrap();
	}
	ZeroingString::from(first)
}

fn prompt_wallet_password(wallet_pass: &Option<&str>) -> ZeroingString {
	match *wallet_pass {
		Some(wallet_pass) => ZeroingString::from(wallet_pass),
		None => {
			ZeroingString::from(rpassword::prompt_password_stdout("Wallet password: ").unwrap())
		}
	}
}
