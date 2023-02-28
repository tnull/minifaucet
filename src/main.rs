use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::Service;
use hyper::{body::Incoming as IncomingBody, Request, Response};

use bitcoin::util::address::Address;
use bitcoin::Amount;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use hyper::Method;
use tokio::net::TcpListener;

use bitcoin::network::constants::Network;
use bitcoin::hashes::Hash;
use ldk_node::{Builder, Config, Node};
use lightning::ln::PaymentHash;
use lightning_invoice::Invoice;

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

    let addr: SocketAddr = listening_address
        .parse()
        .expect("Couldn't parse listening address");

    let listener = TcpListener::bind(addr).await?;

    let rpc_client = Arc::new(
        Client::new(&rpc_url, Auth::CookieFile(cookie_file_path.into()))
            .expect("Couldn't connect RPC client"),
    );

    let mut config = Config::default();
    config.esplora_server_url = esplora_url.to_string();
    config.network = Network::Regtest;

    let builder = Builder::from_config(config);

    let node = Arc::new(Mutex::new(builder.build()));
    let _ = node.lock().unwrap().start();

    let passphrases = Arc::new(vec!["testasdf".to_string()]);

    loop {
        let node = Arc::clone(&node);
        let passphrases = Arc::clone(&passphrases);
        let (stream, _) = listener.accept().await?;

        let client = Arc::clone(&rpc_client);
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(
                    stream,
                    FaucetSvc::new(client, sats_per_request, node, passphrases),
                )
                .await
            {
                println!("Error serving connection: {:?}", err);
            }
        });
    }
}

struct FaucetSvc {
    rpc_client: Arc<Client>,
    sats_per_request: u64,
    node: Arc<Mutex<Node>>,
    passphrases: Arc<Vec<String>>,
    paymenthash_tracking: Mutex<HashMap<PaymentHash, (String, SystemTime)>>,
    passphrase_to_invoice: Mutex<HashMap<String, Invoice>>,
}

impl FaucetSvc {
    pub fn new(
        rpc_client: Arc<Client>,
        sats_per_request: u64,
        node: Arc<Mutex<Node>>,
        passphrases: Arc<Vec<String>>,
    ) -> Self {
        let paymenthash_tracking = Mutex::new(HashMap::new());
        let passphrase_to_invoice = Mutex::new(HashMap::new());
        Self {
            rpc_client,
            sats_per_request,
            node,
            passphrases,
			paymenthash_tracking,
            passphrase_to_invoice,
        }
    }
}

impl Service<Request<IncomingBody>> for FaucetSvc {
    type Response = Response<Full<Bytes>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&mut self, req: Request<IncomingBody>) -> Self::Future {
        fn mk_response(s: String) -> <FaucetSvc as Service<Request<IncomingBody>>>::Future {
            Box::pin(async { Ok(Response::builder().body(Full::new(Bytes::from(s))).unwrap()) })
        }

        fn default_response() -> <FaucetSvc as Service<Request<IncomingBody>>>::Future {
            mk_response("Try /faucet/BITCOIN_ADDRESS to get some regtest bitcoin!".to_string())
        }

        if req.method() != Method::GET {
            return default_response();
        }

        let mut url_parts = req.uri().path().split('/').skip(1);
        println!("url_parts: {:?}", url_parts.clone().collect::<Vec<_>>());
        match url_parts.next() {
            Some("faucet") => {
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
                    let passphrase = passphrase.to_string();
                    if self.passphrases.contains(&passphrase) {
                        let mut invoice_map = self.passphrase_to_invoice.lock().unwrap();
                        let mut paymenthash_map = self.paymenthash_tracking.lock().unwrap();
                        let invoice = if let Some(invoice) = invoice_map.get(&passphrase) {
                            invoice.clone()
                        } else {
                            let invoice = self
                                .node
                                .lock()
                                .unwrap()
                                .receive_payment(Some(10000000), &passphrase, 7200)
                                .unwrap();
                            invoice_map.insert(passphrase.clone(), invoice.clone());
                            paymenthash_map.insert(
                                PaymentHash(invoice.payment_hash().clone().into_inner()),
                                (passphrase.clone(), SystemTime::now()),
                            );
                            invoice
                        };

                        let msg = format!("Hi {}! Please pay invoice: {}", passphrase, invoice);
                        println!("{}", msg);
                        return mk_response(msg);
                    }
                }
            }
            Some("getnodeaddress") => {
                let msg = format!("Node ID: {}", self.node.lock().unwrap().node_id().unwrap());
                println!("{}", msg);
                return mk_response(msg);
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
