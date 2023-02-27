# minifaucet

Minimal faucet to distribute some sats.

## Usage
First run `bitcoind`, e.g.:

```shell
bitcoind -regtest -fallbackfee=0.00001
```

Then run `minifaucet`:

```shell
minifaucet SATS_PER_REQUEST LISTEN_ADDR RPC_URL COOKIE_FILE_PATH
```

You can then reach a webserver at the given `LISTEN_ADDR`. Go to
`http://LISTEN_ADDR/YOUR_BTC_ADDRESS` to get some sats sats, e.g.,
`http://127.0.0.1:3000/bcrt1qmlkulu8hladzejj9gdcjp26cyhq9ql30s7c4ad`.
