// Copyright 2023 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(clippy::expect_used)]

use std::{env, io, io::Write, time::Duration};

use bonsai_sdk_alpha::alpha::Client as AlphaClient;
use bonsai_starter_methods::GUEST_LIST;
use clap::Parser;
use risc0_zkvm::{recursion::SessionRollupReceipt, Executor, ExecutorEnv};

/// Runs the RISC-V ELF binary.
#[derive(Parser)]
#[clap(about, version, author)]
struct Args {
    /// The name of the guest binary
    guest_binary: String,

    /// The input to provide to the guest binary
    input: Option<String>,
}

fn prove_locally(elf: &[u8], input: Vec<u8>) -> Vec<u8> {
    let env = ExecutorEnv::builder().add_input(&input).build();
    let mut exec = Executor::from_elf(env, elf).expect("Failed to instantiate executor");
    let session = exec.run().expect("Failed to run executor");
    // Locally prove resulting journal
    if env::var("PROVE_LOCALLY").is_ok() {
        session.prove().expect("Failed to prove session");
    }
    session.journal
}

const POLL_INTERVAL_SEC: u64 = 4;

#[derive(serde::Deserialize)]
pub struct AlphaRes {
    pub alpha: bool,
}

async fn alpha_selector() -> bool {
    if let Ok(backend) = env::var("BONSAI_BACKEND") {
        backend == "alpha"
    } else {
        let endpoint = env::var("BONSAI_ENDPOINT").expect("Missing BONSAI_ENDPOINT env var");
        let parts = endpoint.split('|').collect::<Vec<&str>>();
        if parts.len() != 2 {
            panic!("Invalid BONSAI_ENDPOINT env var format, expected: '<api_url>|<api_key'");
        }
        let api_key = parts[1];

        let client = reqwest::Client::new();
        let res: AlphaRes = client
            .get("https://36c2brqrq4.execute-api.us-west-2.amazonaws.com/stage/alpha")
            .header("x-api-key", api_key)
            .send()
            .await
            .expect("Failed to get /alpha route")
            .json()
            .await
            .expect("Failed to deserialize alpha response");

        res.alpha
    }
}

fn prove_alpha(elf: &[u8], input: Vec<u8>) -> Vec<u8> {
    let client = AlphaClient::from_env().expect("Failed to create client from env var");

    let img_id = client
        .upload_img(elf.to_vec())
        .expect("Failed to upload ELF image");

    let input_id = client
        .upload_input(input)
        .expect("Failed to upload input data");

    let session = client
        .create_session(img_id, input_id)
        .expect("Failed to create remote proving session");

    loop {
        let res = match session.status(&client) {
            Ok(res) => res,
            Err(err) => {
                eprint!("Failed to get session status: {err}");
                std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SEC));
                continue;
            }
        };
        match res.status.as_str() {
            "RUNNING" => {
                std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SEC));
            }
            "SUCCEEDED" => {
                let receipt_buf = client
                    .download(
                        &res.receipt_url
                            .expect("Missing 'receipt_url' on status response"),
                    )
                    .expect("Failed to download receipt");
                let receipt: SessionRollupReceipt = bincode::deserialize(&receipt_buf)
                    .expect("Failed to deserialize SessionRollupReceipt");
                return receipt.journal;
            }
            _ => {
                panic!("Proving session exited with bad status: {}", res.status);
            }
        }
    }
}

#[tokio::main]
pub async fn main() {
    // Parse arguments
    let args = Args::parse();
    // Search list for requested binary name
    let potential_guest_image_id: [u8; 32] =
        match hex::decode(args.guest_binary.to_lowercase().trim_start_matches("0x")) {
            Ok(byte_vector) => byte_vector.try_into().unwrap_or([0u8; 32]),
            Err(_) => [0u8; 32],
        };
    let guest_entry = GUEST_LIST
        .iter()
        .find(|entry| {
            entry.name == args.guest_binary.to_uppercase()
                || bytemuck::cast::<[u32; 8], [u8; 32]>(entry.image_id) == potential_guest_image_id
        })
        .expect("Unknown guest binary");
    // Execute or return image id
    let output_bytes = match &args.input {
        Some(input) => {
            let input = hex::decode(&input[2..]).expect("Failed to decode input");
            match env::var("BONSAI_ENDPOINT") {
                Ok(_) => {
                    if alpha_selector().await {
                        tokio::task::spawn_blocking(move || prove_alpha(guest_entry.elf, input))
                            .await
                            .expect("Failed to run alpha sub-task")
                    } else {
                        panic!("unsupported backend");
                    }
                }
                Err(_) => prove_locally(guest_entry.elf, input),
            }
        }
        None => Vec::from(bytemuck::cast::<[u32; 8], [u8; 32]>(guest_entry.image_id)),
    };
    let output = hex::encode(output_bytes);
    print!("{output}");
    io::stdout().flush().expect("Failed to flush stdout buffer");
}
