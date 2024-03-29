use std::collections::hash_map;
use std::collections::HashMap;
use std::env;
use std::fmt::Write;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::Service;
use hyper::{body::Incoming as IncomingBody, Request, Response};
use rand::seq::SliceRandom;

use bitcoin::secp256k1::PublicKey;
use bitcoin::util::address::Address;
use bitcoin::Amount;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use hyper::Method;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use bitcoin::hashes::Hash;
use bitcoin::network::constants::Network;
use ldk_node::io::sqlite_store::SqliteStore;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::{Builder, Config, Event, LogLevel, Node};
use lightning_invoice::Bolt11Invoice;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
	let args: Vec<String> = env::args().collect();
	if args.len() < 6 {
		eprintln!(
			"USAGE: {} SATS_PER_REQUEST LISTEN_ADDR RPC_URL ESPLORA_URL COOKIE_FILE_PATH",
			args[0]
		);
		std::process::exit(-1);
	}

	let sats_per_request = u64::from_str(&args[1]).expect("Couldn't parse sats amount");
	let listening_address = &args[2];
	let rpc_url = &args[3];
	let esplora_url = &args[4];
	let cookie_file_path = &args[5];

	let wordlist = Arc::new(read_wordlist("./bip39-wordlist.txt").expect("Couldn't read wordlist"));

	let users: Arc<Mutex<HashMap<String, UserState>>> = Arc::new(Mutex::new(HashMap::new()));

	let addr: SocketAddr = listening_address.parse().expect("Couldn't parse listening address");

	let listener = TcpListener::bind(addr).await?;

	let rpc_client = Arc::new(
		Client::new(&rpc_url, Auth::CookieFile(cookie_file_path.into()))
			.expect("Couldn't connect RPC client"),
	);

	let mut config = Config::default();
	config.network = Network::Regtest;
	config.listening_addresses = Some(vec!["0.0.0.0:9736".parse().unwrap()]);
	config.wallet_sync_interval_secs = 15;
	config.onchain_wallet_sync_interval_secs = 15;
	config.fee_rate_cache_update_interval_secs = 15;
	config.log_level = LogLevel::Trace;

	let mut builder = Builder::from_config(config);
	builder.set_esplora_server(esplora_url.clone());

	let node = Arc::new(builder.build().unwrap());
	node.start()?;

	let node_ref = Arc::clone(&node);

	let shutdown = Arc::new(AtomicBool::new(false));
	let shutdown_ev = Arc::clone(&shutdown);

	let users_ref = Arc::clone(&users);

	tokio::task::spawn(async move {
		loop {
			if shutdown_ev.load(Ordering::Relaxed) {
				break;
			}
			let node = Arc::clone(&node_ref);
			let users = Arc::clone(&users_ref);
			match node.wait_next_event() {
				Event::PaymentReceived { payment_hash, amount_msat } => {
					println!("Received payment: {:?} of amount {}", payment_hash, amount_msat);
					let mut users = users.lock().unwrap();
					for (_, user) in users.iter_mut() {
						if let Some(invoice) = user.invoice() {
							if invoice.payment_hash().into_inner() == payment_hash.0 {
								user.paid_invoice()
							}
						}
					}
					node.event_handled();
				}
				e => {
					println!("Event: {:?}", e);
					node.event_handled();
				}
			}
		}
	});

	loop {
		if shutdown.load(Ordering::Relaxed) {
			break;
		}
		let node = Arc::clone(&node);
		let users = Arc::clone(&users);
		let (stream, _) = listener.accept().await?;

		let client = Arc::clone(&rpc_client);
		let wordlist = Arc::clone(&wordlist);
		let io = TokioIo::new(stream);

		tokio::task::spawn(async move {
			if let Err(err) = http1::Builder::new()
				.serve_connection(
					io,
					FaucetSvc::new(client, sats_per_request, node, users, wordlist),
				)
				.await
			{
				println!("Error serving connection: {:?}", err);
			}
		});
	}
	shutdown.store(true, Ordering::SeqCst);
	node.stop()?;
	Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UserState {
	New,
	CreatedInvoice { invoice: Bolt11Invoice, time_created: SystemTime },
	PaidInvoice { invoice: Bolt11Invoice, time_created: SystemTime, time_paid: SystemTime },
}

impl UserState {
	pub fn created_invoice(&mut self, invoice: Bolt11Invoice) {
		if let UserState::New = self {
			let time_created = SystemTime::now();
			*self = Self::CreatedInvoice { invoice, time_created }
		}
	}

	pub fn paid_invoice(&mut self) {
		if let UserState::CreatedInvoice { invoice, time_created } = self {
			let time_paid = SystemTime::now();
			*self = Self::PaidInvoice {
				invoice: invoice.clone(),
				time_created: time_created.clone(),
				time_paid,
			};
		}
	}

	pub fn invoice(&self) -> Option<&Bolt11Invoice> {
		if let UserState::CreatedInvoice { invoice, time_created: _ } = self {
			return Some(invoice);
		}
		if let UserState::PaidInvoice { invoice, time_created: _, time_paid: _ } = self {
			return Some(invoice);
		}
		None
	}

	pub fn time_diff(&self) -> Option<Duration> {
		if let UserState::PaidInvoice { invoice: _, time_created, time_paid } = self {
			return time_paid.duration_since(*time_created).ok();
		}
		None
	}
}

struct FaucetSvc {
	rpc_client: Arc<Client>,
	sats_per_request: u64,
	node: Arc<Node<SqliteStore>>,
	users: Arc<Mutex<HashMap<String, UserState>>>,
	wordlist: Arc<Vec<String>>,
}

impl FaucetSvc {
	pub fn new(
		rpc_client: Arc<Client>, sats_per_request: u64, node: Arc<Node<SqliteStore>>,
		users: Arc<Mutex<HashMap<String, UserState>>>, wordlist: Arc<Vec<String>>,
	) -> Self {
		Self { rpc_client, sats_per_request, node, users, wordlist }
	}

	fn new_user(&self) -> String {
		let mut user_lock = self.users.lock().unwrap();
		let mut new_passphrase = generate_passphrase(Arc::clone(&self.wordlist));
		while user_lock.contains_key(&new_passphrase) {
			new_passphrase = generate_passphrase(Arc::clone(&self.wordlist));
		}
		user_lock.insert(new_passphrase.clone(), UserState::New);
		new_passphrase
	}
}

impl Service<Request<IncomingBody>> for FaucetSvc {
	type Response = Response<Full<Bytes>>;
	type Error = hyper::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn call(&self, req: Request<IncomingBody>) -> Self::Future {
		fn mk_response(s: String) -> <FaucetSvc as Service<Request<IncomingBody>>>::Future {
			let msg = format!("<html><style>body {{ font-family: \"Lucida Console\", \"Courier New\", monospace; }}</style><head></head><body>{}</body></html>", s);
			Box::pin(async { Ok(Response::builder().body(Full::new(Bytes::from(msg))).unwrap()) })
		}

		fn default_response() -> <FaucetSvc as Service<Request<IncomingBody>>>::Future {
			let msg = format!(
				"<pre>Usage:
	/getsats/BITCOIN_ADDRESS		... to get some sats
	/getinvoice/PASSPHRASE			... to get a payable invoice and start the challenge
	/getchannel/NODE_ID@IP_ADDR:PORT	... to have a channel opened to you
	/getnodeid				... to get the faucet's node id
	/getfundingaddress			... to get the faucet's funding address
	/getleaderboard				... to show the leaderboard
	/payinvoice/INVOICE			... to have the faucet pay an invoice (e.g., to balance a channel)</pre>"
			);
			mk_response(msg)
		}

		if req.method() != Method::GET {
			return default_response();
		}

		let mut url_parts = req.uri().path().split('/').skip(1);
		println!("url_parts: {:?}", url_parts.clone().collect::<Vec<_>>());
		match url_parts.next() {
			Some("getsats") => {
				if let Some(addr_str) = url_parts.next() {
					match Address::from_str(&addr_str) {
						Ok(addr) => {
							let amount = Amount::from_sat(self.sats_per_request);
							let rpc_res = self
								.rpc_client
								.send_to_address(&addr, amount, None, None, None, None, None, None);
							match rpc_res {
								Ok(txid) => {
									let msg = format!("OK: {}", txid);
									println!("{}", msg);
									return mk_response(msg);
								}
								Err(err) => {
									let msg = format!(
										"ERR: {} for request: \"{}\"",
										err,
										req.uri().path()
									);
									eprintln!("{}", msg);
									return mk_response(msg);
								}
							}
						}
						Err(err) => {
							let msg = format!("ERR: {} for request: \"{}\"", err, req.uri().path());
							eprintln!("{}", msg);
							return mk_response(msg);
						}
					}
				}
			}
			Some("getinvoice") => {
				if let Some(passphrase) = url_parts.next() {
					let mut users = self.users.lock().unwrap();
					let pass = format!("{}", passphrase);
					println!("PASS: {:?}", pass);
					let passphrase = passphrase.to_string();
					match users.entry(passphrase.clone()) {
						hash_map::Entry::Occupied(mut user_entry) => {
							if let Some(time_diff) = user_entry.get().time_diff() {
								let msg = format!(
									"DONE! You paid the invoice in {} seconds!",
									time_diff.as_secs()
								);
								println!("{}", msg);
								return mk_response(msg);
							}

							let invoice = if let Some(invoice) = user_entry.get().invoice() {
								invoice.clone()
							} else {
								let invoice =
									self.node.receive_payment(10000000, &passphrase, 7200).unwrap();
								println!("Generated invoice with hash: {}", invoice.payment_hash());
								user_entry.get_mut().created_invoice(invoice.clone());
								invoice
							};

							let msg = format!(
								"<meta http-equiv=\"refresh\" content=\"5\" />Hi {}! Please pay this invoice as quickly as possible:<br><br>{}",
								passphrase, invoice
								);
							println!("{}", msg);
							return mk_response(msg);
						}
						_ => {}
					}
				} else {
					let new_passphrase = self.new_user();
					let msg = format!(
						"<meta http-equiv=\"refresh\" content=\"0; URL=./getinvoice/{}\" />Generated new passphrase: {}",
						new_passphrase, new_passphrase,
						);
					println!("{}", msg);
					return mk_response(msg);
				}
			}
			Some("getchannel") => {
				if let Some(node_address) = url_parts.next() {
					match convert_peer_info(node_address) {
						Ok((pubkey, addr)) => {
							match self.node.connect_open_channel(
								pubkey,
								addr,
								self.sats_per_request,
								Some(self.sats_per_request * 1000 / 2),
								None,
								true,
							) {
								Ok(_) => {
									let msg = format!(
										"Opening channel of {}sat to: {}",
										self.sats_per_request, node_address
									);
									println!("{}", msg);
									return mk_response(msg);
								}
								Err(e) => {
									let msg = format!("ERR: Channel open failed: {:?}", e);
									eprintln!("{}", msg);
									return mk_response(msg);
								}
							}
						}
						Err(()) => {
							let msg = format!("ERR: Failed to parse peer info: {}", node_address);
							eprintln!("{}", msg);
							return mk_response(msg);
						}
					}
				}
			}
			Some("getfundingaddress") => {
				let msg = format!("{}", self.node.new_onchain_address().unwrap());
				println!("{}", msg);
				return mk_response(msg);
			}
			Some("getnodeid") => {
				let msg = format!("{}", self.node.node_id());
				println!("{}", msg);
				return mk_response(msg);
			}
			Some("getleaderboard") => {
				let users = self.users.lock().unwrap();
				let mut leaderboard = Vec::new();
				for (passphrase, user) in users.iter() {
					if let UserState::PaidInvoice { .. } = user {
						leaderboard.push((passphrase, user.time_diff().unwrap()));
					}
				}
				leaderboard.sort_by(|a, b| a.1.cmp(&b.1));

				let mut msg = "<center><meta http-equiv=\"refresh\" content=\"5\" /><table style=\"margin-left:auto;margin-right:auto;text-align: center;\"><tr style=\"border-bottom: 1px solid black\"><th style=\"padding: 10px\">Passphrase</th><th style=\"padding:10px\">Time (sec.)</th></tr>".to_string();
				for (passphrase, time_diff) in leaderboard {
					let row = format!(
                        "<tr><td style=\"padding: 10px\">{}</td><td style=\"padding: 10px\">{}</td>",
                        passphrase,
                        time_diff.as_secs()
                    );
					msg += &row;
				}
				msg += "</table></center>";
				println!("{}", msg);
				return mk_response(msg);
			}
			Some("payinvoice") => {
				let users = self.users.lock().unwrap();
				if let Some(raw_invoice) = url_parts.next() {
					if let Ok(invoice) = Bolt11Invoice::from_str(raw_invoice) {
						for (_, user) in users.iter() {
							if let Some(known_invoice) = user.invoice() {
								// Return so we don't pay our own invoices.
								if invoice.payment_hash().into_inner()
									== known_invoice.payment_hash().into_inner()
								{
									let msg = format!("Won't pay my own invoices!");
									println!("{}", msg);
									return mk_response(msg);
								}
							}
						}
						let msg = match self.node.send_payment(&invoice) {
							Ok(payment_hash) => format!(
								"Paying invoice with hash: {}",
								bytes_to_hex(&payment_hash.0)
							),
							Err(e) => format!("Failed to pay invoice: {}", e),
						};
						println!("{}", msg);
						return mk_response(msg);
					}
				}
				return default_response();
			}
			Some(path) => {
				let msg = format!("ERR: Couldn't find path {}", path);
				eprintln!("{}", msg);
				return default_response();
			}
			None => {
				return default_response();
			}
		}
		default_response()
	}
}

pub fn convert_peer_info(peer_pubkey_and_ip_addr: &str) -> Result<(PublicKey, SocketAddress), ()> {
	if let Some((pubkey_str, peer_str)) = peer_pubkey_and_ip_addr.split_once('@') {
		if let Some(pubkey) = hex_to_compressed_pubkey(pubkey_str) {
			let peer_addr = peer_str.parse().map_err(|_| ())?;
			return Ok((pubkey, peer_addr));
		}
	}
	Err(())
}

fn hex_to_compressed_pubkey(hex: &str) -> Option<PublicKey> {
	if hex.len() != 33 * 2 {
		return None;
	}
	let data = match hex_to_vec(&hex[0..33 * 2]) {
		Some(bytes) => bytes,
		None => return None,
	};
	match PublicKey::from_slice(&data) {
		Ok(pk) => Some(pk),
		Err(_) => None,
	}
}

fn hex_to_vec(hex: &str) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(hex.len() / 2);

	let mut b = 0;
	for (idx, c) in hex.as_bytes().iter().enumerate() {
		b <<= 4;
		match *c {
			b'A'..=b'F' => b |= c - b'A' + 10,
			b'a'..=b'f' => b |= c - b'a' + 10,
			b'0'..=b'9' => b |= c - b'0',
			_ => return None,
		}
		if (idx & 1) == 1 {
			out.push(b);
			b = 0;
		}
	}

	Some(out)
}

#[inline]
fn bytes_to_hex(value: &[u8]) -> String {
	let mut res = String::with_capacity(2 * value.len());
	for v in value {
		write!(&mut res, "{:02x}", v).expect("Unable to write");
	}
	res
}

fn read_wordlist(path: &str) -> Result<Vec<String>, std::io::Error> {
	let mut res = Vec::new();
	for line in std::fs::read_to_string(path)?.lines() {
		res.push(line.to_string());
	}
	Ok(res)
}

fn generate_passphrase(wordlist: Arc<Vec<String>>) -> String {
	let first = wordlist.choose(&mut rand::thread_rng()).unwrap();
	let second = wordlist.choose(&mut rand::thread_rng()).unwrap();
	let third = wordlist.choose(&mut rand::thread_rng()).unwrap();
	format!("{}-{}-{}", first, second, third)
}
