use std::env;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 5 {
        eprintln!(
            "USAGE: {} SATS_PER_REQUEST LISTEN_ADDR RPC_URL COOKIE_FILE_PATH",
            args[0]
        );
        std::process::exit(-1);
    }

    let sats_per_request = u64::from_str(&args[1]).expect("Couldn't parse sats amount");
    let listening_address = &args[2];
    let rpc_url = &args[3];
    let cookie_file_path = &args[4];

    let addr: SocketAddr = listening_address
        .parse()
        .expect("Couldn't parse listening address");

    let listener = TcpListener::bind(addr).await?;

    let rpc_client = Arc::new(
        Client::new(&rpc_url, Auth::CookieFile(cookie_file_path.into()))
            .expect("Couldn't connect RPC client"),
    );

    loop {
        let (stream, _) = listener.accept().await?;

        let client = Arc::clone(&rpc_client);
        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            // Finally, we bind the incoming connection to our `hello` service
            if let Err(err) = http1::Builder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(
                    stream,
                    FaucetSvc {
                        rpc_client: client,
                        sats_per_request,
                    },
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
}

impl Service<Request<IncomingBody>> for FaucetSvc {
    type Response = Response<Full<Bytes>>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&mut self, req: Request<IncomingBody>) -> Self::Future {
        fn mk_response(s: String) -> Result<Response<Full<Bytes>>, hyper::Error> {
            Ok(Response::builder().body(Full::new(Bytes::from(s))).unwrap())
        }

        fn default_response() -> Result<Response<Full<Bytes>>, hyper::Error> {
            Ok(Response::builder()
                .body(Full::new(Bytes::from(
                    "Try /BITCOIN_ADDRESS to get some regtest bitcoin!",
                )))
                .unwrap())
        }

        let res = match (req.method(), req.uri().path()) {
            (&Method::GET, "/") => default_response(),
            (&Method::GET, get_str) => {
                if get_str.len() < 50 {
                    if get_str.starts_with("/") {
                        match Address::from_str(&get_str[1..]) {
                            Ok(addr) => {
                                let amount = Amount::from_sat(self.sats_per_request);
                                let rpc_res = self.rpc_client.send_to_address(
                                    &addr, amount, None, None, None, None, None, None,
                                );
                                match rpc_res {
                                    Ok(txid) => mk_response(format!("OK: {}", txid)),
                                    Err(err) => mk_response(format!("ERR: {}", err)),
                                }
                            }
                            Err(err) => mk_response(format!("ERR: {}", err)),
                        }
                    } else {
                        default_response()
                    }
                } else {
                    default_response()
                }
            }
            _ => default_response(),
        };
        Box::pin(async { res })
    }
}
