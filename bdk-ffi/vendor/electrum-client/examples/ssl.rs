// SPDX-License-Identifier: MIT OR Apache-2.0

extern crate electrum_client;

use electrum_client::{Client, ElectrumApi};

fn main() {
    let client = Client::new("ssl://electrum.blockstream.info:50002").unwrap();
    let res = client.server_features();
    println!("{:#?}", res);
}
