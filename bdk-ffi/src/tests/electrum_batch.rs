//! TALIA FORK: the vendored bdk_electrum 0.24.1 asks for the transactions a sync needs, the
//! previous transactions for fees and headers it fetches again in batched requests
//! (vendor/bdk_electrum.patched, decided 2026-10-25), with the same results as upstream's one
//! request per transaction.
//!
//! A fake Electrum server on 127.0.0.1 serves a made-up chain (headers with real merkle roots,
//! merkle proofs, transactions, script histories and unspent coins, the last for
//! `electrum_script.rs`), reached through electrum-client's real `Client` over TCP, so the
//! batches are written and read as against a public server. Every
//! scenario runs twice, each time from a new client and a new server: once as shipped, and once
//! through `Probe` with `batch_call` refused. The patch's only new request is `batch_call`, and a
//! refused one warms nothing, so the second run is upstream's code path exactly: the same
//! transaction-by-transaction requests, the same results. The two responses must be identical
//! field for field (the transactions in the same order); the shipped run must ask for the same
//! transactions in fewer round trips, never more than 100 in one batch.
//!
//! The faults: a request refused inside a batch, answers out of order (an error answer then
//! reaches electrum-client by its other route), a missing transaction, a transaction with another
//! txid, a previous output that does not exist, a request never answered (a timeout), the
//! connection closed in the middle of a batch, and headers that went stale in a reorg.

use bdk_electrum::bdk_core::spk_client::{
    FullScanRequest, FullScanResponse, SyncRequest, SyncResponse,
};
use bdk_electrum::bdk_core::{CheckPoint, ConfirmationBlockTime, TxUpdate};
use bdk_electrum::electrum_client::{
    self, Batch, BroadcastPackageRes, Client, ConfigBuilder, ElectrumApi, Error, EstimationMode,
    GetBalanceRes, GetHeadersRes, GetHistoryRes, GetMerkleRes, ListUnspentRes, MempoolInfoRes,
    Param, RawHeaderNotification, ScriptStatus, ServerFeaturesRes, TxidFromPosRes,
};
use bdk_electrum::BdkElectrumClient;
use bdk_wallet::bitcoin::block::{Header, Version as BlockVersion};
use bdk_wallet::bitcoin::consensus::serialize;
use bdk_wallet::bitcoin::hashes::{sha256, sha256d, Hash};
use bdk_wallet::bitcoin::hex::DisplayHex;
use bdk_wallet::bitcoin::{
    absolute, transaction, Amount, BlockHash, CompactTarget, OutPoint, Script, ScriptBuf, Sequence,
    Transaction, TxIn, TxMerkleNode, TxOut, Txid, WPubkeyHash, Witness,
};
use bdk_wallet::chain::BlockId;
use bdk_wallet::serde_json::{self, json, Value};
use bdk_wallet::KeychainKind;

use std::borrow::Borrow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub(super) const EXT: u8 = 0xe0;
const INT: u8 = 0x1a;
pub(super) const FOREIGN: u8 = 0xf0;
const START_TIME: u64 = 1_800_000_000;

// ------------------------------------------------------------------ the made-up chain

/// A script of the wallet (`EXT`, `INT`) or of someone else (`FOREIGN`).
pub(super) fn spk(tag: u8, index: u32) -> ScriptBuf {
    let mut hash = [0u8; 20];
    hash[0] = tag;
    hash[1..5].copy_from_slice(&index.to_le_bytes());
    ScriptBuf::new_p2wpkh(&WPubkeyHash::from_byte_array(hash))
}

#[derive(Clone)]
pub(super) struct Chain {
    /// Index = height; each block's transactions in block order.
    blocks: Vec<Vec<Transaction>>,
    /// Salt in each header's time, so a reorged block gets another hash.
    salts: Vec<u32>,
    pub(super) mempool: Vec<Transaction>,
}

pub(super) struct ChainBuilder {
    pub(super) chain: Chain,
    next: u32,
}

impl ChainBuilder {
    pub(super) fn new() -> Self {
        let mut builder = ChainBuilder {
            chain: Chain {
                blocks: vec![],
                salts: vec![],
                mempool: vec![],
            },
            next: 1,
        };
        builder.confirm(vec![]);
        builder
    }

    /// A transaction no other has the txid of (its lock time is a counter).
    pub(super) fn tx(&mut self, inputs: &[OutPoint], outputs: &[(ScriptBuf, u64)]) -> Transaction {
        self.next += 1;
        Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::from_consensus(self.next),
            input: inputs
                .iter()
                .map(|previous_output| TxIn {
                    previous_output: *previous_output,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::from_slice(&[vec![0x51u8; 3]]),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|(script_pubkey, value)| TxOut {
                    value: Amount::from_sat(*value),
                    script_pubkey: script_pubkey.clone(),
                })
                .collect(),
        }
    }

    /// Someone else's coin of `value`, its parent never on the server (only the wallet's own
    /// transactions and their parents are ever asked for).
    pub(super) fn foreign_coin(&mut self, value: u64) -> Transaction {
        let mut nowhere = [0xddu8; 32];
        nowhere[..4].copy_from_slice(&self.next.to_le_bytes());
        let parent = OutPoint::new(Txid::from_byte_array(nowhere), 0);
        let index = self.next;
        self.tx(&[parent], &[(spk(FOREIGN, index), value)])
    }

    /// A block: a coinbase paying someone else, then `txs`.
    pub(super) fn confirm(&mut self, txs: Vec<Transaction>) -> u32 {
        let height = self.chain.blocks.len() as u32;
        let mut coinbase = self.tx(&[OutPoint::null()], &[(spk(FOREIGN, 0x0c00 + height), 50)]);
        coinbase.input[0].witness = Witness::new();
        let mut block = vec![coinbase];
        block.extend(txs);
        self.chain.blocks.push(block);
        self.chain.salts.push(0);
        height
    }

    pub(super) fn empty_blocks(&mut self, n: u32) {
        for _ in 0..n {
            self.confirm(vec![]);
        }
    }
}

fn sha256d_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(a);
    data[32..].copy_from_slice(b);
    sha256d::Hash::hash(&data).to_byte_array()
}

/// Bitcoin's merkle tree (the last node of an odd level paired with itself), every level.
fn merkle_levels(txids: &[Txid]) -> Vec<Vec<[u8; 32]>> {
    let mut levels = vec![txids.iter().map(|t| t.to_byte_array()).collect::<Vec<_>>()];
    while levels.last().unwrap().len() > 1 {
        let level = levels.last().unwrap();
        let next = level
            .chunks(2)
            .map(|pair| sha256d_pair(&pair[0], pair.get(1).unwrap_or(&pair[0])))
            .collect();
        levels.push(next);
    }
    levels
}

/// What the server knows, made from a `Chain`.
struct Index {
    headers: Vec<Header>,
    txs: HashMap<Txid, Transaction>,
    /// Block height and position of each confirmed transaction.
    places: HashMap<Txid, (u32, usize)>,
    /// Electrum script hash (hex) -> history, confirmed first by height and position, then the
    /// mempool (height 0, or -1 for a transaction with an unconfirmed input, as Electrum servers
    /// report them).
    histories: HashMap<String, Vec<(Txid, i64)>>,
    /// Every output some transaction spends, confirmed or in the mempool.
    spent: HashSet<OutPoint>,
}

fn script_hash(script: &Script) -> String {
    let mut hash = sha256::Hash::hash(script.as_bytes()).to_byte_array();
    hash.reverse();
    hash.to_lower_hex_string()
}

impl Index {
    fn new(chain: &Chain) -> Self {
        let mut headers = Vec::<Header>::new();
        let mut txs = HashMap::new();
        let mut places = HashMap::new();
        let mut histories: HashMap<String, Vec<(Txid, i64)>> = HashMap::new();
        let spent: HashSet<OutPoint> = chain
            .blocks
            .iter()
            .flatten()
            .chain(&chain.mempool)
            .flat_map(|tx| tx.input.iter().map(|input| input.previous_output))
            .collect();
        let mut add = |txs: &HashMap<Txid, Transaction>, tx: &Transaction, height: i64| {
            let txid = tx.compute_txid();
            let mut scripts = BTreeSet::new();
            for out in &tx.output {
                scripts.insert(script_hash(&out.script_pubkey));
            }
            for input in &tx.input {
                if let Some(parent) = txs.get(&input.previous_output.txid) {
                    if let Some(out) = parent.output.get(input.previous_output.vout as usize) {
                        scripts.insert(script_hash(&out.script_pubkey));
                    }
                }
            }
            for script in scripts {
                histories.entry(script).or_default().push((txid, height));
            }
        };
        for (height, block) in chain.blocks.iter().enumerate() {
            let txids: Vec<Txid> = block.iter().map(|tx| tx.compute_txid()).collect();
            let levels = merkle_levels(&txids);
            let root = levels.last().unwrap()[0];
            headers.push(Header {
                version: BlockVersion::TWO,
                prev_blockhash: headers
                    .last()
                    .map(|h| h.block_hash())
                    .unwrap_or_else(BlockHash::all_zeros),
                merkle_root: TxMerkleNode::from_byte_array(root),
                time: 1_700_000_000 + height as u32 * 600 + chain.salts[height],
                bits: CompactTarget::from_consensus(0x207f_ffff),
                nonce: 0,
            });
            for (pos, tx) in block.iter().enumerate() {
                txs.insert(tx.compute_txid(), tx.clone());
                places.insert(tx.compute_txid(), (height as u32, pos));
            }
            for tx in block {
                add(&txs, tx, height as i64);
            }
        }
        for tx in &chain.mempool {
            txs.insert(tx.compute_txid(), tx.clone());
        }
        let waiting: HashSet<Txid> = chain.mempool.iter().map(|tx| tx.compute_txid()).collect();
        for tx in &chain.mempool {
            let unconfirmed_input = tx
                .input
                .iter()
                .any(|input| waiting.contains(&input.previous_output.txid));
            add(&txs, tx, if unconfirmed_input { -1 } else { 0 });
        }
        Index {
            headers,
            txs,
            places,
            histories,
            spent,
        }
    }

    fn genesis(&self) -> CheckPoint {
        CheckPoint::new(BlockId {
            height: 0,
            hash: self.headers[0].block_hash(),
        })
    }

    fn proof(&self, chain: &Chain, txid: &Txid, height: usize) -> Option<Value> {
        let (at, pos) = *self.places.get(txid)?;
        if at as usize != height {
            return None;
        }
        let txids: Vec<Txid> = chain.blocks[height]
            .iter()
            .map(|t| t.compute_txid())
            .collect();
        let levels = merkle_levels(&txids);
        let mut branch = vec![];
        let mut index = pos;
        for level in &levels[..levels.len() - 1] {
            let mut sibling = *level.get(index ^ 1).unwrap_or(&level[index]);
            sibling.reverse();
            branch.push(sibling.to_lower_hex_string());
            index /= 2;
        }
        Some(json!({"block_height": height, "merkle": branch, "pos": pos}))
    }
}

// ------------------------------------------------------------------ the fake server

/// What the server does wrong, per transaction (a "first ask" counts over every connection).
#[derive(Default, Clone)]
pub(super) struct Faults {
    /// The first ask is refused with an error answer; later asks are answered.
    refuse_first: HashSet<Txid>,
    /// Every ask is refused: the server does not have it.
    missing: HashSet<Txid>,
    /// Answered with another transaction.
    wrong: HashMap<Txid, Txid>,
    /// The first ask is never answered (the server goes on with the other requests).
    stall_first: HashSet<Txid>,
    /// Every ask is never answered.
    stall_always: HashSet<Txid>,
    /// At the first ask the server closes the connection without answering anything more.
    close_first: HashSet<Txid>,
    /// Answers to the requests read together are written in reverse order.
    pub(super) reverse: bool,
}

struct ServerState {
    chain: Chain,
    index: Index,
    faults: Faults,
    asked: HashMap<Txid, usize>,
}

#[derive(Clone)]
pub(super) struct Server {
    pub(super) url: String,
    state: Arc<Mutex<ServerState>>,
}

enum Reply {
    Answer(Value),
    Silent,
    Close,
}

impl Server {
    pub(super) fn start(chain: Chain, faults: Faults) -> Server {
        let index = Index::new(&chain);
        let state = Arc::new(Mutex::new(ServerState {
            chain,
            index,
            faults,
            asked: HashMap::new(),
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("tcp://{}", listener.local_addr().unwrap());
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let state = Arc::clone(&shared);
                thread::spawn(move || serve(stream, state));
            }
        });
        Server { url, state }
    }

    /// The chain changes under the clients (a reorg), the faults stay.
    fn set_chain(&self, chain: Chain) {
        let mut state = self.state.lock().unwrap();
        state.index = Index::new(&chain);
        state.chain = chain;
    }
}

fn serve(mut stream: TcpStream, state: Arc<Mutex<ServerState>>) {
    let mut pending = Vec::<u8>::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        // Wait for a whole request, then take whatever else has already arrived with it.
        while !pending.contains(&b'\n') {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => pending.extend_from_slice(&buf[..n]),
            }
        }
        let mut closed = false;
        // Answering out of order needs the whole batch at hand: give the rest of a batch that
        // came in more than one piece a moment to arrive.
        if state.lock().unwrap().faults.reverse {
            thread::sleep(Duration::from_millis(20));
        }
        stream.set_nonblocking(true).unwrap();
        loop {
            match stream.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return,
            }
        }
        stream.set_nonblocking(false).unwrap();
        let mut answers = vec![];
        while let Some(end) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let request: Value = serde_json::from_slice(&line).unwrap();
            match answer(&state, &request) {
                Reply::Answer(value) => answers.push(value),
                Reply::Silent => {}
                Reply::Close => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return;
                }
            }
        }
        if state.lock().unwrap().faults.reverse {
            answers.reverse();
        }
        for value in answers {
            let mut out = serde_json::to_vec(&value).unwrap();
            out.push(b'\n');
            if stream.write_all(&out).is_err() {
                return;
            }
        }
        if closed {
            return;
        }
    }
}

fn answer(state: &Arc<Mutex<ServerState>>, request: &Value) -> Reply {
    let mut state = state.lock().unwrap();
    let id = request["id"].clone();
    let params = request["params"].as_array().cloned().unwrap_or_default();
    let ok = |result: Value| Reply::Answer(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let refuse = |code: i64, message: String| {
        Reply::Answer(
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        )
    };
    let header_hex = |state: &ServerState, height: usize| {
        serialize(&state.index.headers[height]).to_lower_hex_string()
    };
    let tip = state.index.headers.len() - 1;
    match request["method"].as_str().unwrap() {
        "server.version" => ok(json!(["fake-electrum 1.0", "1.4"])),
        "server.ping" => ok(Value::Null),
        "blockchain.headers.subscribe" => {
            ok(json!({"height": tip, "hex": header_hex(&state, tip)}))
        }
        "blockchain.block.headers" => {
            let start = params[0].as_u64().unwrap() as usize;
            let count = params[1].as_u64().unwrap() as usize;
            let end = (start + count).min(tip + 1);
            let hex: String = (start..end).map(|h| header_hex(&state, h)).collect();
            ok(json!({"count": end.saturating_sub(start), "hex": hex, "max": 2016}))
        }
        "blockchain.block.header" => {
            let height = params[0].as_u64().unwrap() as usize;
            if height > tip {
                return refuse(1, format!("height {height} out of range"));
            }
            ok(json!(header_hex(&state, height)))
        }
        "blockchain.scripthash.get_history" => {
            let script = params[0].as_str().unwrap();
            let history: Vec<Value> = state
                .index
                .histories
                .get(script)
                .map(|h| {
                    h.iter()
                        .map(
                            |(txid, height)| json!({"tx_hash": txid.to_string(), "height": height}),
                        )
                        .collect()
                })
                .unwrap_or_default();
            ok(Value::Array(history))
        }
        "blockchain.scripthash.listunspent" => {
            // The script's outputs in its history that nothing spends, in history order; a coin
            // from the mempool has height 0 (never -1 here, as Electrum servers answer).
            let script = params[0].as_str().unwrap();
            let index = &state.index;
            let mut unspent = vec![];
            for (txid, height) in index.histories.get(script).into_iter().flatten() {
                for (vout, out) in index.txs[txid].output.iter().enumerate() {
                    let outpoint = OutPoint::new(*txid, vout as u32);
                    if script_hash(&out.script_pubkey) == script && !index.spent.contains(&outpoint)
                    {
                        unspent.push(json!({
                            "tx_hash": txid.to_string(),
                            "tx_pos": vout,
                            "height": (*height).max(0),
                            "value": out.value.to_sat(),
                        }));
                    }
                }
            }
            ok(Value::Array(unspent))
        }
        "blockchain.transaction.get" => {
            let txid = Txid::from_str(params[0].as_str().unwrap()).unwrap();
            let asked = {
                let n = state.asked.entry(txid).or_insert(0);
                *n += 1;
                *n
            };
            let faults = &state.faults;
            if faults.close_first.contains(&txid) && asked == 1 {
                return Reply::Close;
            }
            if faults.stall_always.contains(&txid)
                || (faults.stall_first.contains(&txid) && asked == 1)
            {
                return Reply::Silent;
            }
            if faults.refuse_first.contains(&txid) && asked == 1 {
                return refuse(-101, format!("busy, ask again later: {txid}"));
            }
            if faults.missing.contains(&txid) {
                return refuse(
                    -5,
                    format!("No such mempool or blockchain transaction: {txid}"),
                );
            }
            let served = faults.wrong.get(&txid).copied().unwrap_or(txid);
            match state.index.txs.get(&served) {
                Some(tx) => ok(json!(serialize(tx).to_lower_hex_string())),
                None => refuse(
                    -5,
                    format!("No such mempool or blockchain transaction: {txid}"),
                ),
            }
        }
        "blockchain.transaction.get_merkle" => {
            let txid = Txid::from_str(params[0].as_str().unwrap()).unwrap();
            let height = params[1].as_u64().unwrap() as usize;
            match state.index.proof(&state.chain, &txid, height) {
                Some(proof) => ok(proof),
                None => refuse(1, format!("{txid} is not in block {height}")),
            }
        }
        other => refuse(-32601, format!("unknown method {other}")),
    }
}

// ------------------------------------------------------------------ the counting client

/// One request of the client, as the probe saw it.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    /// `blockchain.transaction.get` on its own.
    Tx(Txid),
    /// A `batch_call` of `blockchain.transaction.get` requests.
    TxBatch(Vec<Txid>),
    /// `blockchain.block.header` on its own.
    Header(usize),
    /// A `batch_call` of `blockchain.block.header` requests.
    HeaderBatch(Vec<u32>),
    /// A `batch_call` the probe refused (upstream's path): no request reached the server.
    Refused,
    /// Any other request (a non-empty batch counts once).
    Other(&'static str),
}

/// electrum-client's `Client`, every request logged; with `refuse_batch_calls`, `batch_call`
/// answers an error without asking the server, which leaves the patched bdk_electrum on
/// upstream's code path.
struct Probe {
    inner: Client,
    refuse_batch_calls: bool,
    calls: Mutex<Vec<Call>>,
}

impl Probe {
    fn log(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }
}

fn param_string(param: &Param) -> Option<&str> {
    match param {
        Param::String(s) => Some(s),
        _ => None,
    }
}

impl ElectrumApi for Probe {
    fn raw_call(
        &self,
        method_name: &str,
        params: impl IntoIterator<Item = Param>,
    ) -> Result<Value, Error> {
        self.log(Call::Other("raw_call"));
        self.inner.raw_call(method_name, params)
    }

    fn batch_call(&self, batch: &Batch) -> Result<Vec<Value>, Error> {
        if self.refuse_batch_calls {
            self.log(Call::Refused);
            return Err(Error::Protocol(json!(
                "batch_call refused by the test probe"
            )));
        }
        let calls: Vec<&(String, Vec<Param>)> = batch.iter().collect();
        if calls
            .iter()
            .all(|(method, _)| method == "blockchain.transaction.get")
        {
            self.log(Call::TxBatch(
                calls
                    .iter()
                    .map(|(_, params)| Txid::from_str(param_string(&params[0]).unwrap()).unwrap())
                    .collect(),
            ));
        } else if calls
            .iter()
            .all(|(method, _)| method == "blockchain.block.header")
        {
            self.log(Call::HeaderBatch(
                calls
                    .iter()
                    .map(|(_, params)| match params[0] {
                        Param::U32(h) => h,
                        _ => panic!("a header batch with a height that is not a u32"),
                    })
                    .collect(),
            ));
        } else {
            self.log(Call::Other("batch_call"));
        }
        self.inner.batch_call(batch)
    }

    fn block_headers_subscribe_raw(&self) -> Result<RawHeaderNotification, Error> {
        self.log(Call::Other("headers.subscribe"));
        self.inner.block_headers_subscribe_raw()
    }

    fn block_headers_pop_raw(&self) -> Result<Option<RawHeaderNotification>, Error> {
        self.inner.block_headers_pop_raw()
    }

    fn block_header_raw(&self, height: usize) -> Result<Vec<u8>, Error> {
        self.log(Call::Header(height));
        self.inner.block_header_raw(height)
    }

    fn block_headers(&self, start_height: usize, count: usize) -> Result<GetHeadersRes, Error> {
        self.log(Call::Other("block.headers"));
        self.inner.block_headers(start_height, count)
    }

    fn estimate_fee(&self, number: usize, mode: Option<EstimationMode>) -> Result<f64, Error> {
        self.log(Call::Other("estimatefee"));
        self.inner.estimate_fee(number, mode)
    }

    fn relay_fee(&self) -> Result<f64, Error> {
        self.log(Call::Other("relayfee"));
        self.inner.relay_fee()
    }

    fn script_subscribe(&self, script: &Script) -> Result<Option<ScriptStatus>, Error> {
        self.log(Call::Other("scripthash.subscribe"));
        self.inner.script_subscribe(script)
    }

    fn batch_script_subscribe<'s, I>(&self, scripts: I) -> Result<Vec<Option<ScriptStatus>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.log(Call::Other("batch scripthash.subscribe"));
        self.inner.batch_script_subscribe(scripts)
    }

    fn script_unsubscribe(&self, script: &Script) -> Result<bool, Error> {
        self.log(Call::Other("scripthash.unsubscribe"));
        self.inner.script_unsubscribe(script)
    }

    fn script_pop(&self, script: &Script) -> Result<Option<ScriptStatus>, Error> {
        self.inner.script_pop(script)
    }

    fn script_get_balance(&self, script: &Script) -> Result<GetBalanceRes, Error> {
        self.log(Call::Other("scripthash.get_balance"));
        self.inner.script_get_balance(script)
    }

    fn batch_script_get_balance<'s, I>(&self, scripts: I) -> Result<Vec<GetBalanceRes>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.log(Call::Other("batch scripthash.get_balance"));
        self.inner.batch_script_get_balance(scripts)
    }

    fn script_get_history(&self, script: &Script) -> Result<Vec<GetHistoryRes>, Error> {
        self.log(Call::Other("scripthash.get_history"));
        self.inner.script_get_history(script)
    }

    fn batch_script_get_history<'s, I>(&self, scripts: I) -> Result<Vec<Vec<GetHistoryRes>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        if scripts.clone().into_iter().next().is_some() {
            self.log(Call::Other("batch scripthash.get_history"));
        }
        self.inner.batch_script_get_history(scripts)
    }

    fn script_list_unspent(&self, script: &Script) -> Result<Vec<ListUnspentRes>, Error> {
        self.log(Call::Other("scripthash.listunspent"));
        self.inner.script_list_unspent(script)
    }

    fn batch_script_list_unspent<'s, I>(
        &self,
        scripts: I,
    ) -> Result<Vec<Vec<ListUnspentRes>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'s Script>,
    {
        self.log(Call::Other("batch scripthash.listunspent"));
        self.inner.batch_script_list_unspent(scripts)
    }

    fn transaction_get_raw(&self, txid: &Txid) -> Result<Vec<u8>, Error> {
        self.log(Call::Tx(*txid));
        self.inner.transaction_get_raw(txid)
    }

    fn batch_transaction_get_raw<'t, I>(&self, txids: I) -> Result<Vec<Vec<u8>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<&'t Txid>,
    {
        self.log(Call::Other("batch transaction.get (raw)"));
        self.inner.batch_transaction_get_raw(txids)
    }

    fn batch_block_header_raw<I>(&self, heights: I) -> Result<Vec<Vec<u8>>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<u32>,
    {
        if heights.clone().into_iter().next().is_some() {
            self.log(Call::Other("batch block.header (raw)"));
        }
        self.inner.batch_block_header_raw(heights)
    }

    fn batch_estimate_fee<I>(&self, numbers: I) -> Result<Vec<f64>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<usize>,
    {
        self.log(Call::Other("batch estimatefee"));
        self.inner.batch_estimate_fee(numbers)
    }

    fn transaction_broadcast_raw(&self, raw_tx: &[u8]) -> Result<Txid, Error> {
        self.log(Call::Other("transaction.broadcast"));
        self.inner.transaction_broadcast_raw(raw_tx)
    }

    fn transaction_broadcast_package_raw<T: AsRef<[u8]>>(
        &self,
        raw_txs: &[T],
    ) -> Result<BroadcastPackageRes, Error> {
        self.log(Call::Other("transaction.broadcast_package"));
        self.inner.transaction_broadcast_package_raw(raw_txs)
    }

    fn transaction_get_merkle(&self, txid: &Txid, height: usize) -> Result<GetMerkleRes, Error> {
        self.log(Call::Other("transaction.get_merkle"));
        self.inner.transaction_get_merkle(txid, height)
    }

    fn batch_transaction_get_merkle<I>(
        &self,
        txids_and_heights: I,
    ) -> Result<Vec<GetMerkleRes>, Error>
    where
        I: IntoIterator + Clone,
        I::Item: Borrow<(Txid, usize)>,
    {
        if txids_and_heights.clone().into_iter().next().is_some() {
            self.log(Call::Other("batch transaction.get_merkle"));
        }
        self.inner.batch_transaction_get_merkle(txids_and_heights)
    }

    fn txid_from_pos(&self, height: usize, tx_pos: usize) -> Result<Txid, Error> {
        self.log(Call::Other("transaction.id_from_pos"));
        self.inner.txid_from_pos(height, tx_pos)
    }

    fn txid_from_pos_with_merkle(
        &self,
        height: usize,
        tx_pos: usize,
    ) -> Result<TxidFromPosRes, Error> {
        self.log(Call::Other("transaction.id_from_pos (merkle)"));
        self.inner.txid_from_pos_with_merkle(height, tx_pos)
    }

    fn server_features(&self) -> Result<ServerFeaturesRes, Error> {
        self.log(Call::Other("server.features"));
        self.inner.server_features()
    }

    fn mempool_get_info(&self) -> Result<MempoolInfoRes, Error> {
        self.log(Call::Other("mempool.get_info"));
        self.inner.mempool_get_info()
    }

    fn ping(&self) -> Result<(), Error> {
        self.log(Call::Other("server.ping"));
        self.inner.ping()
    }

    fn calls_made(&self) -> Result<usize, Error> {
        self.inner.calls_made()
    }
}

// ------------------------------------------------------------------ running a scenario

/// How the client connects.
#[derive(Clone, Copy)]
struct Connect {
    timeout_s: u8,
    retry: u8,
}

const DEFAULT_CONNECT: Connect = Connect {
    timeout_s: 5,
    retry: 0,
};

fn client(server: &Server, connect: Connect, upstream: bool) -> BdkElectrumClient<Probe> {
    let config = ConfigBuilder::new()
        .timeout(Some(Duration::from_secs(connect.timeout_s.into())))
        .retry(connect.retry)
        .build();
    let inner = Client::from_config(&server.url, config).unwrap();
    BdkElectrumClient::new(Probe {
        inner,
        refuse_batch_calls: upstream,
        calls: Mutex::new(vec![]),
    })
}

fn calls(client: &BdkElectrumClient<Probe>) -> Vec<Call> {
    std::mem::take(&mut *client.inner.calls.lock().unwrap())
}

/// Everything a response says, in one comparable text: the transactions in their order (with
/// duplicates), floating outputs, anchors, seen and evicted times, last active indexes, the
/// chain update's blocks.
fn view_tx_update(tx_update: &TxUpdate<ConfirmationBlockTime>) -> Vec<String> {
    let mut lines = vec![];
    for tx in &tx_update.txs {
        lines.push(format!(
            "tx {}",
            serialize(tx.as_ref()).to_lower_hex_string()
        ));
    }
    for (outpoint, txout) in &tx_update.txouts {
        lines.push(format!(
            "txout {outpoint} {} {}",
            txout.value.to_sat(),
            txout.script_pubkey.to_hex_string()
        ));
    }
    for (anchor, txid) in &tx_update.anchors {
        lines.push(format!(
            "anchor {txid} {} {} {}",
            anchor.block_id.height, anchor.block_id.hash, anchor.confirmation_time
        ));
    }
    let seen: BTreeSet<_> = tx_update.seen_ats.iter().collect();
    for (txid, at) in seen {
        lines.push(format!("seen {txid} {at}"));
    }
    let evicted: BTreeSet<_> = tx_update.evicted_ats.iter().collect();
    for (txid, at) in evicted {
        lines.push(format!("evicted {txid} {at}"));
    }
    lines
}

fn view_chain(chain_update: &Option<CheckPoint>) -> Vec<String> {
    match chain_update {
        None => vec!["chain none".to_string()],
        Some(tip) => tip
            .iter()
            .map(|cp| format!("block {} {}", cp.height(), cp.hash()))
            .collect(),
    }
}

fn view_full_scan(response: &FullScanResponse<KeychainKind>) -> Vec<String> {
    let mut lines = view_tx_update(&response.tx_update);
    for (keychain, index) in &response.last_active_indices {
        lines.push(format!("last active {keychain:?} {index}"));
    }
    lines.extend(view_chain(&response.chain_update));
    lines
}

fn view_sync(response: &SyncResponse) -> Vec<String> {
    let mut lines = view_tx_update(&response.tx_update);
    lines.extend(view_chain(&response.chain_update));
    lines
}

/// The wallet's scripts on both keychains, `0..count` each.
fn keychain_spks(tag: u8, count: u32) -> Vec<(u32, ScriptBuf)> {
    (0..count).map(|i| (i, spk(tag, i))).collect()
}

fn full_scan_request(index: &Index, external: u32, internal: u32) -> FullScanRequest<KeychainKind> {
    FullScanRequest::builder_at(START_TIME)
        .chain_tip(index.genesis())
        .spks_for_keychain(KeychainKind::External, keychain_spks(EXT, external))
        .spks_for_keychain(KeychainKind::Internal, keychain_spks(INT, internal))
        .build()
}

/// A sync of the scripts `0..count` of both keychains, each with the transactions `expected`
/// says the wallet already knows for it.
fn sync_request(
    index: &Index,
    count: u32,
    expected: &[(ScriptBuf, Txid)],
) -> SyncRequest<(KeychainKind, u32)> {
    let mut spks = vec![];
    for i in 0..count {
        spks.push(((KeychainKind::External, i), spk(EXT, i)));
        spks.push(((KeychainKind::Internal, i), spk(INT, i)));
    }
    SyncRequest::builder_at(START_TIME + 60)
        .chain_tip(index.genesis())
        .spks_with_indexes(spks)
        .expected_spk_txids(expected.iter().cloned())
        .build()
}

/// The transactions a run asked the server for, each time (single asks and batched asks).
fn asked_txids(calls: &[Call]) -> Vec<Txid> {
    let mut out = vec![];
    for call in calls {
        match call {
            Call::Tx(txid) => out.push(*txid),
            Call::TxBatch(txids) => out.extend(txids.iter().copied()),
            _ => {}
        }
    }
    out
}

fn round_trips(calls: &[Call]) -> usize {
    calls.iter().filter(|c| **c != Call::Refused).count()
}

fn tx_batches(calls: &[Call]) -> Vec<usize> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::TxBatch(txids) => Some(txids.len()),
            _ => None,
        })
        .collect()
}

fn singles(calls: &[Call]) -> usize {
    calls.iter().filter(|c| matches!(c, Call::Tx(_))).count()
}

/// One run: a full scan, then a sync of the same scripts on the same client.
struct Run {
    full_scan: Result<Vec<String>, String>,
    full_scan_calls: Vec<Call>,
    sync: Option<Result<Vec<String>, String>>,
    sync_calls: Vec<Call>,
}

#[derive(Clone)]
struct Scenario {
    chain: Chain,
    faults: Faults,
    connect: Connect,
    external: u32,
    internal: u32,
    stop_gap: usize,
    batch_size: usize,
    fetch_prev_txouts: bool,
    /// A sync after the full scan (the coverage pass of Talia's bridge), with these expected
    /// transactions per script.
    sync: Option<(u32, Vec<(ScriptBuf, Txid)>)>,
}

fn run(scenario: &Scenario, upstream: bool) -> Run {
    let server = Server::start(scenario.chain.clone(), scenario.faults.clone());
    let index = Index::new(&scenario.chain);
    let client = client(&server, scenario.connect, upstream);
    let full_scan = client
        .full_scan(
            full_scan_request(&index, scenario.external, scenario.internal),
            scenario.stop_gap,
            scenario.batch_size,
            scenario.fetch_prev_txouts,
        )
        .map(|r| view_full_scan(&r))
        .map_err(|e| format!("{:?}", e));
    let full_scan_calls = calls(&client);
    let mut sync = None;
    if let (Ok(_), Some((count, expected))) = (&full_scan, &scenario.sync) {
        sync = Some(
            client
                .sync(
                    sync_request(&index, *count, expected),
                    scenario.batch_size,
                    scenario.fetch_prev_txouts,
                )
                .map(|r| view_sync(&r))
                .map_err(|e| format!("{:?}", e)),
        );
    }
    let sync_calls = calls(&client);
    Run {
        full_scan,
        full_scan_calls,
        sync,
        sync_calls,
    }
}

/// Both runs, the same results; returns (shipped, upstream).
fn same_as_upstream(scenario: &Scenario) -> (Run, Run) {
    let shipped = run(scenario, false);
    let upstream = run(scenario, true);
    assert_eq!(
        shipped.full_scan, upstream.full_scan,
        "full scan differs from upstream's"
    );
    assert_eq!(shipped.sync, upstream.sync, "sync differs from upstream's");
    (shipped, upstream)
}

fn set(txids: &[Txid]) -> BTreeSet<Txid> {
    txids.iter().copied().collect()
}

// ------------------------------------------------------------------ the chains

/// A wallet with a history like the phone's test vaults: `receives` payments from outside to
/// receive addresses 0.., each from its own outside coin (a previous transaction for the fee),
/// some three to a block, some still in the mempool; a coinbase paying the wallet; `spends`
/// payments out of the wallet with change; a payment between the wallet's own addresses.
struct History {
    chain: Chain,
    /// (script, txid) of every wallet transaction, for a sync's expected txids.
    expected: Vec<(ScriptBuf, Txid)>,
}

fn history(receives: u32, spends: u32) -> History {
    let mut b = ChainBuilder::new();
    b.empty_blocks(3);
    let mut expected = vec![];
    let mut coins = vec![];
    let mut pending = vec![];
    for i in 0..receives {
        let parent = b.foreign_coin(100_000 + u64::from(i));
        let receive = b.tx(
            &[OutPoint::new(parent.compute_txid(), 0)],
            &[
                (spk(FOREIGN, 0x0100 + i), 1_000),
                (spk(EXT, i), 50_000 + u64::from(i)),
            ],
        );
        expected.push((spk(EXT, i), receive.compute_txid()));
        coins.push(OutPoint::new(receive.compute_txid(), 1));
        pending.push(parent);
        pending.push(receive);
        if pending.len() >= 6 {
            b.confirm(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        b.confirm(std::mem::take(&mut pending));
    }
    // A coinbase paying the wallet (no previous transaction to fetch).
    let mut coinbase = b.tx(&[OutPoint::null()], &[(spk(EXT, receives), 5_000_000)]);
    coinbase.input[0].witness = Witness::new();
    expected.push((spk(EXT, receives), coinbase.compute_txid()));
    b.chain.blocks.push(vec![coinbase]);
    b.chain.salts.push(0);
    // Payments out, each with change; then one between the wallet's own addresses.
    for j in 0..spends {
        let coin = coins.remove(0);
        let pay = b.tx(
            &[coin],
            &[(spk(FOREIGN, 0x0200 + j), 20_000), (spk(INT, j), 29_000)],
        );
        expected.push((spk(INT, j), pay.compute_txid()));
        coins.push(OutPoint::new(pay.compute_txid(), 1));
        b.confirm(vec![pay]);
    }
    if coins.len() >= 2 {
        let a = coins.remove(0);
        let c = coins.remove(0);
        let own = b.tx(&[a, c], &[(spk(EXT, 1), 70_000), (spk(INT, spends), 1_000)]);
        expected.push((spk(INT, spends), own.compute_txid()));
        b.confirm(vec![own]);
    }
    b.empty_blocks(10);
    // In the mempool: a receive from an outside coin that is itself waiting.
    let parent = b.foreign_coin(77_777);
    let waiting = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(spk(EXT, receives + 1), 60_000)],
    );
    expected.push((spk(EXT, receives + 1), waiting.compute_txid()));
    b.chain.mempool.push(parent);
    b.chain.mempool.push(waiting);
    History {
        chain: b.chain,
        expected,
    }
}

fn scenario(history: &History) -> Scenario {
    Scenario {
        chain: history.chain.clone(),
        faults: Faults::default(),
        connect: DEFAULT_CONNECT,
        external: 400,
        internal: 400,
        stop_gap: 20,
        batch_size: 25,
        fetch_prev_txouts: true,
        sync: Some((60, history.expected.clone())),
    }
}

// ------------------------------------------------------------------ the tests

#[test]
fn the_fake_server_serves_proofs_electrum_client_accepts() {
    let h = history(5, 2);
    let index = Index::new(&h.chain);
    for (height, block) in h.chain.blocks.iter().enumerate() {
        for tx in block {
            let txid = tx.compute_txid();
            let proof: GetMerkleRes =
                serde_json::from_value(index.proof(&h.chain, &txid, height).unwrap()).unwrap();
            assert!(electrum_client::utils::validate_merkle_proof(
                &txid,
                &index.headers[height].merkle_root,
                &proof
            ));
        }
    }
}

#[test]
fn a_first_sync_matches_upstream_in_fewer_round_trips() {
    let h = history(16, 4);
    let (shipped, upstream) = same_as_upstream(&scenario(&h));
    let found = shipped.full_scan.as_ref().unwrap();
    assert!(found.iter().filter(|l| l.starts_with("tx ")).count() >= 22);
    assert!(found.iter().any(|l| l.starts_with("txout ")));
    assert!(found.iter().any(|l| l.starts_with("anchor ")));
    assert!(found.iter().any(|l| l.starts_with("seen ")));
    // The same transactions asked for, each once, upstream one at a time.
    let asked = asked_txids(&shipped.full_scan_calls);
    assert_eq!(
        asked.len(),
        set(&asked).len(),
        "{:?}",
        shipped.full_scan_calls
    );
    assert_eq!(set(&asked), set(&asked_txids(&upstream.full_scan_calls)));
    assert_eq!(singles(&upstream.full_scan_calls), asked.len());
    assert_eq!(
        singles(&shipped.full_scan_calls),
        0,
        "{:?}",
        shipped.full_scan_calls
    );
    let (ours, theirs) = (
        round_trips(&shipped.full_scan_calls),
        round_trips(&upstream.full_scan_calls),
    );
    assert!(
        ours + asked.len() / 2 <= theirs,
        "round trips: shipped {}, upstream {}",
        ours,
        theirs
    );
    // The sync after it (the bridge's coverage pass) finds everything cached on both paths.
    assert!(asked_txids(&shipped.sync_calls).is_empty());
    assert!(asked_txids(&upstream.sync_calls).is_empty());
    assert!(shipped.sync.unwrap().is_ok());
}

#[test]
fn a_sync_with_a_vanished_transaction_matches_upstream() {
    let h = history(6, 2);
    let mut s = scenario(&h);
    // The wallet expected a transaction at receive address 2 that the server no longer has in
    // that script's history: evicted at the sync's start time, on both paths.
    let mut gone = [0x99u8; 32];
    gone[0] = 0x01;
    let vanished = Txid::from_byte_array(gone);
    s.sync.as_mut().unwrap().1.push((spk(EXT, 2), vanished));
    let (shipped, _) = same_as_upstream(&s);
    let sync = shipped.sync.unwrap().unwrap();
    assert!(sync.contains(&format!("evicted {vanished} {}", START_TIME + 60)));
}

#[test]
fn the_stop_gap_inside_a_batch_still_ends_what_is_fetched() {
    // Receive addresses 0..5 used, then a transaction at 20 that the stop gap (10, reached at
    // 15) never gets to, although 20 is inside the same batch of 25 scripts.
    let mut b = ChainBuilder::new();
    let mut txs = vec![];
    for i in 0..6 {
        let parent = b.foreign_coin(10_000);
        let receive = b.tx(
            &[OutPoint::new(parent.compute_txid(), 0)],
            &[(spk(EXT, i), 9_000)],
        );
        txs.push(parent);
        txs.push(receive);
    }
    let parent = b.foreign_coin(10_000);
    let beyond = b.tx(
        &[OutPoint::new(parent.compute_txid(), 0)],
        &[(spk(EXT, 20), 9_000)],
    );
    let beyond_txid = beyond.compute_txid();
    txs.push(parent);
    txs.push(beyond);
    b.confirm(txs);
    b.empty_blocks(3);
    let s = Scenario {
        chain: b.chain,
        faults: Faults::default(),
        connect: DEFAULT_CONNECT,
        external: 100,
        internal: 0,
        stop_gap: 10,
        batch_size: 25,
        fetch_prev_txouts: true,
        sync: None,
    };
    let (shipped, upstream) = same_as_upstream(&s);
    assert!(!asked_txids(&shipped.full_scan_calls).contains(&beyond_txid));
    assert!(!asked_txids(&upstream.full_scan_calls).contains(&beyond_txid));
    assert!(shipped
        .full_scan
        .unwrap()
        .contains(&"last active External 5".to_string()));
}

#[test]
fn more_than_a_hundred_transactions_go_in_batches_of_at_most_a_hundred() {
    let h = history(230, 3);
    let mut s = scenario(&h);
    s.batch_size = 200;
    s.sync = None;
    let (shipped, upstream) = same_as_upstream(&s);
    let batches = tx_batches(&shipped.full_scan_calls);
    assert!(batches.iter().all(|n| *n <= 100), "{:?}", batches);
    assert!(batches.len() >= 5, "{:?}", batches);
    let asked = asked_txids(&shipped.full_scan_calls);
    assert_eq!(set(&asked), set(&asked_txids(&upstream.full_scan_calls)));
    assert!(round_trips(&shipped.full_scan_calls) * 20 < round_trips(&upstream.full_scan_calls));
}

#[test]
fn a_request_refused_inside_a_batch_is_asked_again_on_its_own() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    let refused = [h.expected[3].1, h.expected[9].1];
    s.faults.refuse_first = refused.iter().copied().collect();
    let shipped = run(&s, false);
    // The server answers in order, so electrum-client fails the batch as a whole at the
    // refusal: its transactions are asked for again one by one, the refused one included.
    for txid in refused {
        assert!(shipped.full_scan_calls.contains(&Call::Tx(txid)));
    }
    // The result is upstream's from a server with no fault.
    let clean = run(&scenario(&h), true);
    assert_eq!(shipped.full_scan, clean.full_scan);
    assert_eq!(shipped.sync, clean.sync);
    // The one way the outcomes can differ: a server that refuses a request once and answers it
    // when asked again. The batched code asks twice (in the batch, then on its own), upstream
    // once, so upstream's sync fails on that refusal.
    let upstream = run(&s, true);
    assert!(upstream.full_scan.unwrap_err().starts_with("Protocol("));
}

#[test]
fn a_refused_request_ends_only_its_own_batch() {
    // More than 100 transactions in one warm-up (a batch of 200 scripts), one of the first 100
    // refused once, in its turn. electrum-client fails that batch as a whole and keeps the
    // connection; the next batch of the same warm-up still goes as a batch. Only the refused
    // batch's transactions are asked for again one by one, and the result is a fault-free run's.
    let h = history(230, 3);
    let mut s = scenario(&h);
    s.batch_size = 200;
    s.sync = None;
    let clean = run(&s, true);
    let refused = h.expected[3].1;
    s.faults.refuse_first = HashSet::from([refused]);
    let shipped = run(&s, false);
    assert_eq!(shipped.full_scan, clean.full_scan);
    let first_batch: BTreeSet<Txid> = match shipped
        .full_scan_calls
        .iter()
        .find(|c| matches!(c, Call::TxBatch(_)))
    {
        Some(Call::TxBatch(txids)) => txids.iter().copied().collect(),
        _ => panic!("no batch: {:?}", shipped.full_scan_calls),
    };
    assert_eq!(first_batch.len(), 100);
    assert!(first_batch.contains(&refused));
    let asked_alone: BTreeSet<Txid> = shipped
        .full_scan_calls
        .iter()
        .filter_map(|c| match c {
            Call::Tx(txid) => Some(*txid),
            _ => None,
        })
        .collect();
    assert_eq!(singles(&shipped.full_scan_calls), 100);
    assert!(asked_alone == first_batch);
    // The same transactions as upstream asks for, each in a batch at most twice (the refused
    // batch, then on its own).
    assert_eq!(
        set(&asked_txids(&shipped.full_scan_calls)),
        set(&asked_txids(&clean.full_scan_calls))
    );
}

#[test]
fn answers_out_of_order_with_a_refusal_inside_still_match_upstream() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    s.faults.reverse = true;
    let refused = h.expected[5].1;
    s.faults.refuse_first = HashSet::from([refused]);
    let shipped = run(&s, false);
    // Read before its turn, the refusal reaches electrum-client by its other route and arrives
    // as a null answer, which the warm-up leaves out: that transaction alone is asked again.
    assert!(shipped.full_scan_calls.contains(&Call::Tx(refused)));
    assert!(
        singles(&shipped.full_scan_calls) < 4,
        "{:?}",
        shipped.full_scan_calls
    );
    let clean = run(&scenario(&h), true);
    assert_eq!(shipped.full_scan, clean.full_scan);
    assert_eq!(shipped.sync, clean.sync);
}

#[test]
fn a_missing_transaction_is_still_the_same_error() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    // Two missing: the error names the first one upstream asks for, in the server's words.
    s.faults.missing = HashSet::from([h.expected[7].1, h.expected[4].1]);
    let (shipped, _) = same_as_upstream(&s);
    let error = shipped.full_scan.unwrap_err();
    assert!(error.starts_with("Protocol("), "{}", error);
    assert!(error.contains(&h.expected[4].1.to_string()), "{}", error);
}

#[test]
fn a_missing_previous_transaction_is_still_the_same_error() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    // The outside coin of receive 6 and of receive 2.
    let parents: Vec<Txid> = [6usize, 2]
        .iter()
        .map(|i| {
            let receive = h
                .chain
                .blocks
                .iter()
                .flatten()
                .find(|tx| tx.compute_txid() == h.expected[*i].1);
            receive.unwrap().input[0].previous_output.txid
        })
        .collect();
    s.faults.missing = parents.iter().copied().collect();
    let (shipped, _) = same_as_upstream(&s);
    let error = shipped.full_scan.unwrap_err();
    assert!(error.starts_with("Protocol("), "{}", error);
}

#[test]
fn a_transaction_with_another_txid_is_still_the_same_error() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    s.faults.wrong = HashMap::from([(h.expected[8].1, h.expected[1].1)]);
    let (shipped, _) = same_as_upstream(&s);
    let error = shipped.full_scan.unwrap_err();
    assert!(
        error.contains("electrum server returned transaction with unexpected txid"),
        "{}",
        error
    );
}

#[test]
fn a_previous_output_that_does_not_exist_is_still_the_same_error() {
    let mut b = ChainBuilder::new();
    let parent = b.foreign_coin(10_000);
    let other_parent = b.foreign_coin(20_000);
    // Spends output 7 of a parent that has one output.
    let receive = b.tx(
        &[
            OutPoint::new(other_parent.compute_txid(), 0),
            OutPoint::new(parent.compute_txid(), 7),
        ],
        &[(spk(EXT, 0), 9_000)],
    );
    b.confirm(vec![parent, other_parent, receive]);
    b.empty_blocks(2);
    let s = Scenario {
        chain: b.chain,
        faults: Faults::default(),
        connect: DEFAULT_CONNECT,
        external: 30,
        internal: 0,
        stop_gap: 20,
        batch_size: 25,
        fetch_prev_txouts: true,
        sync: None,
    };
    let (shipped, _) = same_as_upstream(&s);
    let error = shipped.full_scan.unwrap_err();
    assert!(error.contains("does not exist"), "{}", error);
}

#[test]
fn a_request_left_unanswered_in_a_batch_costs_a_timeout_then_works() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    s.connect = Connect {
        timeout_s: 1,
        retry: 0,
    };
    s.faults.stall_first = HashSet::from([h.expected[6].1]);
    let shipped = run(&s, false);
    // The batch timed out, so each of its transactions was asked for again on its own, and the
    // result is a fault-free run's.
    assert!(shipped.full_scan_calls.contains(&Call::Tx(h.expected[6].1)));
    let clean = run(&scenario(&h), true);
    assert_eq!(shipped.full_scan, clean.full_scan);
    assert_eq!(shipped.sync, clean.sync);
}

#[test]
fn a_request_never_answered_fails_as_upstream_fails() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    s.connect = Connect {
        timeout_s: 1,
        retry: 0,
    };
    s.faults.stall_always = HashSet::from([h.expected[6].1]);
    let (shipped, _) = same_as_upstream(&s);
    let error = shipped.full_scan.unwrap_err();
    assert!(error.starts_with("AllAttemptsErrored("), "{}", error);
}

#[test]
fn a_connection_closed_during_a_batch() {
    let h = history(16, 4);
    let mut s = scenario(&h);
    s.faults.close_first = HashSet::from([h.expected[6].1]);
    // No retry: both fail with electrum-client's error for a connection that failed with no
    // retry left (it keeps no error in the list then).
    let (shipped, _) = same_as_upstream(&s);
    assert_eq!(shipped.full_scan.unwrap_err(), "AllAttemptsErrored([])");
    // With a retry, electrum-client reconnects and both finish with the same result.
    s.connect = Connect {
        timeout_s: 5,
        retry: 1,
    };
    let (shipped, _) = same_as_upstream(&s);
    let clean = run(&scenario(&h), false);
    assert_eq!(shipped.full_scan, clean.full_scan);
}

#[test]
fn headers_that_went_stale_are_fetched_again_in_one_batch() {
    // Chain A: wallet payments X1, X2 at heights 6 and 8. Chain B: the same blocks 6 and 8
    // also confirm Y1, Y2 (a reorg at 6), with 20 blocks after, so the client's cached headers
    // at 6 and 8 are stale and outside the 8 latest headers every sync refreshes.
    let mut b = ChainBuilder::new();
    b.empty_blocks(5);
    let make = |b: &mut ChainBuilder, index: u32| {
        let parent = b.foreign_coin(10_000);
        let pay = b.tx(
            &[OutPoint::new(parent.compute_txid(), 0)],
            &[(spk(EXT, index), 9_000)],
        );
        (parent, pay)
    };
    let (p1, x1) = make(&mut b, 0);
    let (p2, x2) = make(&mut b, 1);
    let (q1, y1) = make(&mut b, 2);
    let (q2, y2) = make(&mut b, 3);
    let mut a = b.chain.clone();
    let mut a_builder = ChainBuilder {
        chain: a.clone(),
        next: b.next,
    };
    a_builder.confirm(vec![p1.clone(), x1.clone()]); // 6
    a_builder.confirm(vec![]); // 7
    a_builder.confirm(vec![p2.clone(), x2.clone()]); // 8
    a_builder.empty_blocks(20);
    a = a_builder.chain;
    let mut chain_b = a.clone();
    chain_b.blocks[6].extend([q1, y1.clone()]);
    chain_b.blocks[8].extend([q2, y2.clone()]);
    chain_b.salts[6] = 1;
    chain_b.salts[8] = 1;

    let run_reorg = |upstream: bool| {
        let server = Server::start(a.clone(), Faults::default());
        let client = client(&server, DEFAULT_CONNECT, upstream);
        let first = client
            .full_scan(full_scan_request(&Index::new(&a), 30, 0), 20, 25, false)
            .map(|r| view_full_scan(&r))
            .map_err(|e| format!("{:?}", e));
        calls(&client);
        server.set_chain(chain_b.clone());
        let index_b = Index::new(&chain_b);
        let second = client
            .sync(sync_request(&index_b, 10, &[]), 25, false)
            .map(|r| view_sync(&r))
            .map_err(|e| format!("{:?}", e));
        (first, second, calls(&client), index_b)
    };
    let (first, second, ours, index_b) = run_reorg(false);
    let (first_up, second_up, theirs, _) = run_reorg(true);
    assert_eq!(first, first_up);
    assert_eq!(second, second_up);
    // Y1 and Y2 are anchored in chain B's blocks 6 and 8.
    let second = second.unwrap();
    for (txid, height) in [(y1.compute_txid(), 6usize), (y2.compute_txid(), 8)] {
        let hash = index_b.headers[height].block_hash();
        assert!(
            second
                .iter()
                .any(|l| l.starts_with(&format!("anchor {txid} {height} {hash} "))),
            "{:#?}",
            second
        );
    }
    assert!(ours.contains(&Call::HeaderBatch(vec![6, 8])), "{:?}", ours);
    assert!(
        !ours.iter().any(|c| matches!(c, Call::Header(6 | 8))),
        "{:?}",
        ours
    );
    assert!(theirs.contains(&Call::Header(6)) && theirs.contains(&Call::Header(8)));
    let _ = (x1, x2);
}

#[test]
fn a_lone_missing_transaction_is_left_to_its_own_request() {
    // One wallet transaction with no outside parent: nothing to batch, the same single request
    // upstream makes, and no batch at all.
    let mut b = ChainBuilder::new();
    let mut coinbase = b.tx(&[OutPoint::null()], &[(spk(EXT, 0), 5_000_000)]);
    coinbase.input[0].witness = Witness::new();
    let txid = coinbase.compute_txid();
    b.chain.blocks.push(vec![coinbase]);
    b.chain.salts.push(0);
    b.empty_blocks(2);
    let s = Scenario {
        chain: b.chain,
        faults: Faults::default(),
        connect: DEFAULT_CONNECT,
        external: 30,
        internal: 0,
        stop_gap: 20,
        batch_size: 25,
        fetch_prev_txouts: true,
        sync: None,
    };
    let (shipped, upstream) = same_as_upstream(&s);
    assert_eq!(shipped.full_scan_calls, {
        let mut c = upstream.full_scan_calls.clone();
        c.retain(|c| *c != Call::Refused);
        c
    });
    assert!(shipped.full_scan_calls.contains(&Call::Tx(txid)));
}

#[test]
fn the_kept_client_asks_for_nothing_it_has() {
    // Talia keeps a vault's client between syncs: the second full scan finds every transaction
    // and previous transaction cached, as upstream does.
    let h = history(16, 4);
    let s = scenario(&h);
    let server = Server::start(s.chain.clone(), Faults::default());
    let index = Index::new(&s.chain);
    let client = client(&server, DEFAULT_CONNECT, false);
    let first = client
        .full_scan(full_scan_request(&index, 400, 400), 20, 25, true)
        .map(|r| view_full_scan(&r))
        .unwrap();
    calls(&client);
    let again = client
        .full_scan(full_scan_request(&index, 400, 400), 20, 25, true)
        .map(|r| view_full_scan(&r))
        .unwrap();
    assert_eq!(first, again);
    assert!(asked_txids(&calls(&client)).is_empty());
}

#[test]
fn every_txid_in_a_batch_is_checked_before_it_is_kept() {
    // The wrong transaction comes back inside a batch: it is not cached under the asked txid,
    // so the transaction-by-transaction code asks again and reports the mismatch, as upstream.
    let h = history(16, 4);
    let mut s = scenario(&h);
    s.faults.wrong = HashMap::from([(h.expected[2].1, h.expected[3].1)]);
    s.faults.refuse_first = HashSet::new();
    let shipped = run(&s, false);
    assert!(shipped.full_scan_calls.contains(&Call::Tx(h.expected[2].1)));
    assert!(shipped.full_scan.unwrap_err().contains("unexpected txid"));
}
