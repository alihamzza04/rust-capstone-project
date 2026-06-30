#![allow(unused)]
use bitcoincore_rpc::bitcoin::Amount;
use bitcoincore_rpc::bitcoin::{Network, Txid};
use bitcoincore_rpc::{Auth, Client, RpcApi};
use serde::Deserialize;
use serde_json::json;
use std::fs::File;
use std::io::Write;
use std::str::FromStr;

// Node access params
const RPC_URL: &str = "http://127.0.0.1:18443"; // Default regtest RPC port
const RPC_USER: &str = "alice";
const RPC_PASS: &str = "password";

// You can use calls not provided in RPC lib API using the generic `call` function.
// An example of using the `send` RPC call, which doesn't have exposed API.
// You can also use serde_json `Deserialize` derivation to capture the returned json result.
fn send(rpc: &Client, addr: &str, amount_btc: f64) -> bitcoincore_rpc::Result<String> {
    let args = [
        json!([{addr : amount_btc }]), // recipient address and amount
        json!(null),                   // conf target
        json!(null),                   // estimate mode
        json!(null),                   // fee rate in sats/vb
        json!(null),                   // Empty option object
    ];

    #[derive(Deserialize)]
    struct SendResult {
        complete: bool,
        txid: String,
    }
    let send_result = rpc.call::<SendResult>("send", &args)?;
    assert!(send_result.complete);
    Ok(send_result.txid)
}

fn wallet_client(wallet: &str) -> bitcoincore_rpc::Result<Client> {
    Client::new(
        &format!("{}/wallet/{}", RPC_URL, wallet),
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )
}

fn main() -> bitcoincore_rpc::Result<()> {
    // Connect to Bitcoin Core RPC
    let rpc = Client::new(
        RPC_URL,
        Auth::UserPass(RPC_USER.to_owned(), RPC_PASS.to_owned()),
    )?;

    // Get blockchain info
    let blockchain_info = rpc.get_blockchain_info()?;
    println!("Blockchain Info: {:?}", blockchain_info);

    // Create/Load the wallets, named 'Miner' and 'Trader'. Have logic to optionally create/load them if they do not exist or not loaded already.
    ensure_wallet_loaded(&rpc, "Miner")?;
    ensure_wallet_loaded(&rpc, "Trader")?;

    // Generate spendable balances in the Miner wallet. How many blocks needs to be mined?
    // Coinbase transactions (block rewards) require 100 confirmations before they become spendable
    // in Bitcoin. This is called "coinbase maturity". We mine 101 blocks: the first 100 mature
    // the coinbase, and the 101st gives us a positive spendable balance.

    let miner_client = wallet_client("Miner")?;
    let trader_client = wallet_client("Trader")?;

    let miner_address = miner_client
        .get_new_address(Some("Mining Reward"), None)?
        .require_network(Network::Regtest)
        .expect("Address should be valid for regtest");

    if rpc.get_blockchain_info()?.blocks < 101 {
        println!("Mining 101 blocks to mature coinbase...");
        rpc.generate_to_address(101, &miner_address)?;
    }
    println!("Miner balance: {}", miner_client.get_balance(None, None)?);

    // Load Trader wallet and generate a new address

    let trader_address = trader_client
        .get_new_address(Some("Received"), None)?
        .require_network(Network::Regtest)
        .expect("Address should be valid for regtest");
    println!("Trader address: {}", trader_address);

    // Send 20 BTC from Miner to Trader

    let txid_str = send(&miner_client, &trader_address.to_string(), 20.0)?;
    let txid = Txid::from_str(&txid_str).expect("Invalid txid");
    println!("TXID: {}", txid_str);

    // Check transaction in mempool

    let mempool = rpc.get_raw_mempool()?;
    assert!(mempool.contains(&txid), "TX not in mempool");
    println!("✓ in mempool");

    // Mine 1 block to confirm the transaction

    let block_hash = rpc.generate_to_address(1, &miner_address)?[0];
    println!("✓ confirmed in block {}", block_hash);

    // Extract all required transaction details

    // Get transaction with verbose mode to get block height and hash from RPC response
    #[derive(Deserialize)]
    #[allow(non_snake_case)]
    struct GetTransactionResult {
        blockhash: Option<String>,
        blockheight: Option<i32>,
        fee: Option<f64>,
    }
    let tx_verbose: GetTransactionResult =
        miner_client.call("gettransaction", &[json!(txid), json!(null), json!(true)])?;
    let block_height = tx_verbose.blockheight.unwrap_or(0) as i64;
    let block_hash = tx_verbose.blockhash.unwrap_or_default();
    let rpc_fee_btc = tx_verbose.fee.unwrap_or(0.0).abs();

    let tx_result = miner_client.get_transaction(&txid, None)?;
    let tx = tx_result.transaction()?;

    // Use RPC fee to match test expectations
    let fee_btc = rpc_fee_btc;

    // Get input address from first input using RPC
    let input_txid = tx.input[0].previous_output.txid;
    let input_vout = tx.input[0].previous_output.vout;
    let input_tx_result = miner_client.get_transaction(&input_txid, None)?;
    let input_tx = input_tx_result.transaction()?;
    let input_amount_sat = input_tx.output[input_vout as usize].value.to_sat();
    let input_amount_btc = input_amount_sat as f64 / 100_000_000.0;

    // Use RPC call to get decoded transaction with addresses
    #[derive(Deserialize)]
    struct Vin {
        address: Option<String>,
    }
    #[allow(non_snake_case)]
    #[derive(Deserialize)]
    struct ScriptPubKey {
        addresses: Option<Vec<String>>,
    }
    #[allow(non_snake_case)]
    #[derive(Deserialize)]
    struct Vout {
        value: f64,
        scriptPubKey: ScriptPubKey,
    }
    #[derive(Deserialize)]
    struct DecodedTx {
        vin: Vec<Vin>,
        vout: Vec<Vout>,
    }
    let decoded_tx: DecodedTx = rpc.call("getrawtransaction", &[json!(txid_str), json!(1)])?;
    let input_address = decoded_tx.vin[0].address.clone().unwrap_or_default();

    // Identify trader output (20 BTC) and change output
    // Use amount-based identification since addresses might be formatted differently
    let mut trader_output_addr = String::new();
    let mut trader_output_amount_btc = 0.0;
    let mut change_addr = String::new();
    let mut change_amount_btc = 0.0;

    for output in &decoded_tx.vout {
        let addr = output
            .scriptPubKey
            .addresses
            .clone()
            .and_then(|a| a.into_iter().next())
            .unwrap_or_default();
        let amount_btc = output.value;
        // The trader output should be close to 20 BTC (the amount we sent)
        if (amount_btc - 20.0).abs() < 0.001 {
            trader_output_addr = addr;
            trader_output_amount_btc = amount_btc;
        } else {
            change_addr = addr;
            change_amount_btc = amount_btc;
        }
    }

    println!("Fee: {fee_btc} BTC | Input: {input_amount_btc} BTC | Trader Output: {trader_output_amount_btc} BTC | Change: {change_amount_btc} BTC");

    // Write the data to ../out.txt in the specified format given in readme.md

    let mut file = File::create("../out.txt")?;
    writeln!(file, "{}", txid_str)?;
    writeln!(file, "{}", input_address)?;
    writeln!(file, "{}", input_amount_btc)?;
    writeln!(file, "{}", trader_output_addr)?;
    writeln!(file, "{}", trader_output_amount_btc)?;
    writeln!(file, "{}", change_addr)?;
    writeln!(file, "{}", change_amount_btc)?;
    writeln!(file, "{}", fee_btc)?;
    writeln!(file, "{}", block_height)?;
    writeln!(file, "{}", block_hash)?;
    println!("✓ written to ../out.txt");

    Ok(())
}

// Helper function to ensure a wallet is loaded
fn ensure_wallet_loaded(rpc: &Client, wallet_name: &str) -> bitcoincore_rpc::Result<()> {
    // Check if wallet is already loaded
    let loaded_wallets = rpc.list_wallets()?;

    if loaded_wallets.contains(&wallet_name.to_string()) {
        println!("Wallet '{}' is already loaded", wallet_name);
        return Ok(());
    }

    // Try to load the wallet
    match rpc.load_wallet(wallet_name) {
        Ok(_) => {
            println!("Loaded existing wallet '{}'", wallet_name);
            Ok(())
        }
        Err(_) => {
            // If loading fails, the wallet might not exist, so create it
            println!("Creating new wallet '{}'", wallet_name);
            rpc.create_wallet(wallet_name, None, None, None, Some(false))?;
            Ok(())
        }
    }
}
