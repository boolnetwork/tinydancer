//! Copyright (c) 2022 Blockworks Foundation
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.

pub mod bridge;
pub mod configs;
pub mod encoding;
pub mod rpc;
pub mod tpu_manager;
pub mod workers;
// pub mod cli;
pub mod block_store;
use crate::convert_to_websocket;
use crate::rpc_wrapper::bridge::LiteBridge;
use crate::tinydancer::{endpoint, ClientService, Cluster};
use anyhow::bail;
use async_trait::async_trait;
use clap::Parser;
use const_env::from_env;
use dotenv::dotenv;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_ledger::shred::Signer;
use solana_sdk::signer::keypair::Keypair;
use solana_transaction_status::TransactionConfirmationStatus;
use std::sync::Arc;
use std::{env, time::Duration};
use tiny_logger::logs::info;
use tokio::task::JoinHandle;

// use self::cli::Args;

#[from_env]
pub const DEFAULT_RPC_ADDR: &str = "http://0.0.0.0:8899";
#[from_env]
pub const DEFAULT_LITE_RPC_ADDR: &str = "http://0.0.0.0:8890";
#[from_env]
pub const DEFAULT_WS_ADDR: &str = "ws://0.0.0.0:8900";
#[from_env]
pub const DEFAULT_TX_MAX_RETRIES: u16 = 1;
#[from_env]
pub const DEFAULT_TX_BATCH_SIZE: usize = 128;
#[from_env]
pub const DEFAULT_FANOUT_SIZE: u64 = 32;
#[from_env]
pub const DEFAULT_TX_BATCH_INTERVAL_MS: u64 = 1;
#[from_env]
pub const DEFAULT_CLEAN_INTERVAL_MS: u64 = 5 * 60 * 1000; // five minute
#[from_env]
pub const DEFAULT_TX_SENT_TTL_S: u64 = 12;
pub const DEFAULT_TRANSACTION_CONFIRMATION_STATUS: TransactionConfirmationStatus =
    TransactionConfirmationStatus::Finalized;

pub struct TransactionService {
    tx_handle: JoinHandle<Result<(), anyhow::Error>>,
}

pub struct TransactionServiceConfig {
    pub cluster: Cluster,
    pub db_instance: Arc<rocksdb::DB>,
}

async fn get_identity_keypair(identity_from_cli: &String) -> Keypair {
    if let Ok(identity_env_var) = env::var("IDENTITY") {
        if let Ok(identity_bytes) = serde_json::from_str::<Vec<u8>>(identity_env_var.as_str()) {
            print!("HASIII TO HASU");
            Keypair::from_bytes(identity_bytes.as_slice()).unwrap()
        } else {
            // must be a file
            let identity_file = tokio::fs::read_to_string(identity_env_var.as_str())
                .await
                .expect("Cannot find the identity file provided");
            let identity_bytes: Vec<u8> = serde_json::from_str(&identity_file).unwrap();
            Keypair::from_bytes(identity_bytes.as_slice()).unwrap()
        }
    } else if identity_from_cli.is_empty() {
        Keypair::new()
    } else {
        let identity_file = tokio::fs::read_to_string(identity_from_cli.as_str())
            .await
            .expect("Cannot find the identity file provided");
        let identity_bytes: Vec<u8> = serde_json::from_str(&identity_file).unwrap();
        Keypair::from_bytes(identity_bytes.as_slice()).unwrap()
    }
}

#[async_trait]
impl ClientService<TransactionServiceConfig> for TransactionService {
    type ServiceError = tokio::io::Error;
    fn new(config: TransactionServiceConfig) -> Self {
        let transaction_handle = tokio::spawn(async {
            dotenv().ok();
            let rpc_url = endpoint(config.cluster);

            let payer = Keypair::new();

            let tx_batch_interval_ms = Duration::from_millis(DEFAULT_TX_BATCH_INTERVAL_MS);
            let clean_interval_ms = Duration::from_millis(DEFAULT_CLEAN_INTERVAL_MS);

            let light_bridge = LiteBridge::new(
                String::from(rpc_url.clone()),
                String::from(convert_to_websocket!(rpc_url)),
                DEFAULT_FANOUT_SIZE,
                payer,
                config.db_instance,
            )
            .await?;

            let services = light_bridge
                .start_services(
                    String::from("[::]:8890"),
                    String::from("[::]:8891"),
                    DEFAULT_TX_BATCH_SIZE,
                    tx_batch_interval_ms,
                    clean_interval_ms,
                )
                .await?;

            let services = futures::future::try_join_all(services);

            let ctrl_c_signal = tokio::signal::ctrl_c();

            tokio::select! {
                _ = services => {
                    bail!("Services quit unexpectedly");
                }
                _ = ctrl_c_signal => {
                    info!("Received ctrl+c signal");
                    Ok(())
                }
            }
        });
        Self {
            tx_handle: transaction_handle,
        }
    }

    async fn join(self) -> std::result::Result<(), Self::ServiceError> {
        let _ = self.tx_handle.await;
        Ok(())
    }
}
