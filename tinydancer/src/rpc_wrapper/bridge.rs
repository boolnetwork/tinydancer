use crate::{
    get_endpoint,
    rpc_wrapper::{
        block_store::{BlockInformation, BlockStore},
        configs::{IsBlockHashValidConfig, SendTransactionConfig},
        encoding::BinaryEncoding,
        rpc::LiteRpcServer,
        tpu_manager::TpuManager,
        workers::{BlockListener, Cleaner, TxSender, WireTransaction},
    },
    sampler::{get_serialized, pull_and_verify_shreds, SHRED_CF},
    tinydancer::Cluster,
    ConfigSchema,
};
use colored::Colorize;
use hyper::Method;
use reqwest::header;
use serde::{self, Deserialize, Serialize};
use solana_client::rpc_response::RpcApiVersion;
use std::{
    fs,
    ops::{Deref, Sub},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::bail;

use solana_ledger::shred::{Shred, ShredType, Slot};
use tiny_logger::logs::{info, warn};

use jsonrpsee::{server::ServerBuilder, types::SubscriptionResult, SubscriptionSink};
use prometheus::{core::GenericGauge, opts, register_int_counter, register_int_gauge, IntCounter};
use solana_rpc_client::{nonblocking::rpc_client::RpcClient, rpc_client::SerializableTransaction};
use solana_rpc_client_api::{
    config::{RpcContextConfig, RpcRequestAirdropConfig, RpcSignatureStatusConfig},
    response::{Response as RpcResponse, RpcBlockhash, RpcResponseContext, RpcVersionInfo},
};
use solana_sdk::{
    blake3::hashv, commitment_config::CommitmentConfig, hash::Hash, pubkey::Pubkey,
    signature::Keypair, transaction::VersionedTransaction,
};
use solana_transaction_status::TransactionStatus;
use tokio::{
    net::ToSocketAddrs,
    sync::mpsc::{self, UnboundedSender},
    task::JoinHandle,
};
use tower_http::cors::{Any, CorsLayer};

lazy_static::lazy_static! {
    static ref RPC_SEND_TX: IntCounter =
    register_int_counter!(opts!("literpc_rpc_send_tx", "RPC call send transaction")).unwrap();
    static ref RPC_GET_LATEST_BLOCKHASH: IntCounter =
    register_int_counter!(opts!("literpc_rpc_get_latest_blockhash", "RPC call to get latest block hash")).unwrap();
    static ref RPC_IS_BLOCKHASH_VALID: IntCounter =
    register_int_counter!(opts!("literpc_rpc_is_blockhash_valid", "RPC call to check if blockhash is vali calld")).unwrap();
    static ref RPC_GET_SIGNATURE_STATUSES: IntCounter =
    register_int_counter!(opts!("literpc_rpc_get_signature_statuses", "RPC call to get signature statuses")).unwrap();
    static ref RPC_GET_VERSION: IntCounter =
    register_int_counter!(opts!("literpc_rpc_get_version", "RPC call to version")).unwrap();
    static ref RPC_REQUEST_AIRDROP: IntCounter =
    register_int_counter!(opts!("literpc_rpc_airdrop", "RPC call to request airdrop")).unwrap();
    static ref RPC_SIGNATURE_SUBSCRIBE: IntCounter =
    register_int_counter!(opts!("literpc_rpc_signature_subscribe", "RPC call to subscribe to signature")).unwrap();
    pub static ref TXS_IN_CHANNEL: GenericGauge<prometheus::core::AtomicI64> = register_int_gauge!(opts!("literpc_txs_in_channel", "Transactions in channel")).unwrap();
}

/// A bridge between clients and tpu
pub struct LiteBridge {
    pub rpc_client: Arc<RpcClient>,
    pub tpu_manager: Arc<TpuManager>,
    pub db_instance: Arc<rocksdb::DB>,
    // None if LiteBridge is not executed
    pub tx_send_channel: Option<UnboundedSender<(String, WireTransaction, u64)>>,
    pub tx_sender: TxSender,
    pub block_listner: BlockListener,
    pub block_store: BlockStore,
}

impl LiteBridge {
    pub async fn new(
        rpc_url: String,
        ws_addr: String,
        fanout_slots: u64,
        identity: Keypair,
        db_instance: Arc<rocksdb::DB>,
    ) -> anyhow::Result<Self> {
        let rpc_client = Arc::new(RpcClient::new(rpc_url.clone()));

        let tpu_manager =
            Arc::new(TpuManager::new(rpc_client.clone(), ws_addr, fanout_slots, identity).await?);

        let tx_sender = TxSender::new(tpu_manager.clone());

        let block_store = BlockStore::new(&rpc_client).await?;

        let block_listner =
            BlockListener::new(rpc_client.clone(), tx_sender.clone(), block_store.clone());

        Ok(Self {
            db_instance,
            rpc_client,
            tpu_manager,
            tx_send_channel: None,
            tx_sender,
            block_listner,
            block_store,
        })
    }

    /// List for `JsonRpc` requests
    #[allow(clippy::too_many_arguments)]
    pub async fn start_services<T: ToSocketAddrs + std::fmt::Debug + 'static + Send + Clone>(
        mut self,
        http_addr: T,
        ws_addr: T,
        tx_batch_size: usize,
        tx_send_interval: Duration,
        clean_interval: Duration,
    ) -> anyhow::Result<Vec<JoinHandle<anyhow::Result<()>>>> {
        let (tx_send, tx_recv) = mpsc::unbounded_channel();
        self.tx_send_channel = Some(tx_send);

        let tx_sender = self
            .tx_sender
            .clone()
            .execute(tx_recv, tx_batch_size, tx_send_interval);

        let finalized_block_listener = self
            .block_listner
            .clone()
            .listen(CommitmentConfig::finalized());

        let confirmed_block_listener = self
            .block_listner
            .clone()
            .listen(CommitmentConfig::confirmed());

        let cleaner = Cleaner::new(
            self.tx_sender.clone(),
            self.block_listner.clone(),
            self.block_store.clone(),
            self.tpu_manager.clone(),
        )
        .start(clean_interval);

        let rpc = self.into_rpc();

        let (ws_server, http_server) = {
            let ws_server_handle = ServerBuilder::default()
                .ws_only()
                .build(ws_addr.clone())
                .await?
                .start(rpc.clone())?;
            let cors = CorsLayer::new()
                .allow_methods([Method::POST, Method::GET])
                .allow_origin(Any)
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    header::ACCESS_CONTROL_ALLOW_METHODS,
                ]);
            let middleware = tower::ServiceBuilder::new().layer(cors);
            let http_server_handle = ServerBuilder::default()
                .http_only()
                .set_middleware(middleware)
                .set_host_filtering(jsonrpsee::server::AllowHosts::Any)
                .build(http_addr.clone())
                .await?
                .start(rpc)?;

            let ws_server = tokio::spawn(async move {
                info!("Websocket Server started at {ws_addr:?}");
                ws_server_handle.stopped().await;
                bail!("Websocket server stopped");
            });

            let http_server = tokio::spawn(async move {
                info!("HTTP Server started at {http_addr:?}");
                http_server_handle.stopped().await;
                bail!("HTTP server stopped");
            });

            (ws_server, http_server)
        };

        let services = vec![
            ws_server,
            http_server,
            tx_sender,
            finalized_block_listener,
            confirmed_block_listener,
            cleaner,
        ];

        Ok(services)
    }
}

#[jsonrpsee::core::async_trait]
impl LiteRpcServer for LiteBridge {
    async fn send_transaction(
        &self,
        tx: String,
        send_transaction_config: Option<SendTransactionConfig>,
    ) -> crate::rpc_wrapper::rpc::Result<String> {
        RPC_SEND_TX.inc();

        let SendTransactionConfig {
            encoding,
            max_retries: _,
        } = send_transaction_config.unwrap_or_default();

        let raw_tx = match encoding.decode(tx) {
            Ok(raw_tx) => raw_tx,
            Err(err) => {
                return Err(jsonrpsee::core::Error::Custom(err.to_string()));
            }
        };

        let tx = match bincode::deserialize::<VersionedTransaction>(&raw_tx) {
            Ok(tx) => tx,
            Err(err) => {
                return Err(jsonrpsee::core::Error::Custom(err.to_string()));
            }
        };

        let sig = tx.get_signature();
        let Some(BlockInformation { slot, .. }) = self
            .block_store
            .get_block_info(&tx.get_recent_blockhash().to_string())
            .await else {
                warn!("block");
                return Err(jsonrpsee::core::Error::Custom("Blockhash not found in block store".to_string()));
        };

        self.tx_send_channel
            .as_ref()
            .expect("Lite Bridge Not Executed")
            .send((sig.to_string(), raw_tx, slot))
            .unwrap();
        TXS_IN_CHANNEL.inc();

        Ok(BinaryEncoding::Base58.encode(sig))
    }

    async fn get_latest_blockhash(
        &self,
        config: Option<RpcContextConfig>,
    ) -> crate::rpc_wrapper::rpc::Result<LiteResponse<RpcBlockhash>> {
        RPC_GET_LATEST_BLOCKHASH.inc();

        let commitment_config = config
            .map(|config| config.commitment.unwrap_or_default())
            .unwrap_or_default();

        let (
            blockhash,
            BlockInformation {
                slot, block_height, ..
            },
        ) = self.block_store.get_latest_block(commitment_config).await;

        info!("glb {blockhash} {slot} {block_height}");
        let mut rpc_url = String::from("http://0.0.0.0:8899");
        let home_path = std::env::var("HOME").unwrap();
        let is_existing = home_path.clone() + "/.config/tinydancer/config.json";
        let path = Path::new(&is_existing);
        if path.exists() {
            let file = fs::File::open(home_path.clone() + "/.config/tinydancer/config.json")
                .expect("Error reading config in bridge");
            let config: ConfigSchema = serde_json::from_reader(file).unwrap();
            rpc_url = get_endpoint(config.cluster);
        } else {
            println!(
                "{} {}",
                "Initialise a config first using:".to_string().yellow(),
                "tinydancer set config".to_string().green()
            );
        }
        let sampled =
            pull_and_verify_shreds(slot as usize, String::from(rpc_url), 10 as usize).await;

        Ok(LiteResponse {
            context: LiteRpcResponseContext {
                slot,
                api_version: None,
                sampled,
            },
            value: RpcBlockhash {
                blockhash,
                last_valid_block_height: block_height + 150,
            },
        })
    }

    async fn is_blockhash_valid(
        &self,
        blockhash: String,
        config: Option<IsBlockHashValidConfig>,
    ) -> crate::rpc_wrapper::rpc::Result<RpcResponse<bool>> {
        RPC_IS_BLOCKHASH_VALID.inc();

        let commitment = config.unwrap_or_default().commitment.unwrap_or_default();
        let commitment = CommitmentConfig { commitment };

        let blockhash = match Hash::from_str(&blockhash) {
            Ok(blockhash) => blockhash,
            Err(err) => {
                return Err(jsonrpsee::core::Error::Custom(err.to_string()));
            }
        };

        let is_valid = match self
            .rpc_client
            .is_blockhash_valid(&blockhash, commitment)
            .await
        {
            Ok(is_valid) => is_valid,
            Err(err) => {
                return Err(jsonrpsee::core::Error::Custom(err.to_string()));
            }
        };

        let slot = self
            .block_store
            .get_latest_block_info(commitment)
            .await
            .slot;

        Ok(RpcResponse {
            context: RpcResponseContext {
                slot,
                api_version: None,
            },
            value: is_valid,
        })
    }

    async fn get_signature_statuses(
        &self,
        sigs: Vec<String>,
        _config: Option<RpcSignatureStatusConfig>,
    ) -> crate::rpc_wrapper::rpc::Result<LiteResponse<Vec<Option<TransactionStatus>>>> {
        RPC_GET_SIGNATURE_STATUSES.inc();

        let sig_statuses = sigs
            .iter()
            .map(|sig| {
                self.tx_sender
                    .txs_sent_store
                    .get(sig)
                    .and_then(|v| v.status.clone())
            })
            .collect();
        let slot = self
            .block_store
            .get_latest_block_info(CommitmentConfig::finalized())
            .await
            .slot;
        let mut rpc_url = String::from("http://0.0.0.0:8899");
        let home_path = std::env::var("HOME").unwrap();
        let is_existing = home_path.clone() + "/.config/tinydancer/config.json";
        let path = Path::new(&is_existing);
        if path.exists() {
            let file = fs::File::open(home_path.clone() + "/.config/tinydancer/config.json")
                .expect("Error reading config in bridge");
            let config: ConfigSchema = serde_json::from_reader(file).unwrap();
            rpc_url = get_endpoint(config.cluster);
        } else {
            println!(
                "{} {}",
                "Initialise a config first using:".to_string().yellow(),
                "tinydancer set config".to_string().green()
            );
        }
        let sampled =
            pull_and_verify_shreds(slot as usize, String::from(rpc_url), 10 as usize).await;
        Ok(LiteResponse {
            context: LiteRpcResponseContext {
                slot,
                api_version: None,
                sampled,
            },
            value: sig_statuses,
        })
    }

    fn get_version(&self) -> crate::rpc_wrapper::rpc::Result<RpcVersionInfo> {
        RPC_GET_VERSION.inc();

        let version = solana_version::Version::default();
        Ok(RpcVersionInfo {
            solana_core: version.to_string(),
            feature_set: Some(version.feature_set),
        })
    }

    async fn request_airdrop(
        &self,
        pubkey_str: String,
        lamports: u64,
        config: Option<RpcRequestAirdropConfig>,
    ) -> crate::rpc_wrapper::rpc::Result<String> {
        RPC_REQUEST_AIRDROP.inc();

        let pubkey = match Pubkey::from_str(&pubkey_str) {
            Ok(pubkey) => pubkey,
            Err(err) => {
                return Err(jsonrpsee::core::Error::Custom(err.to_string()));
            }
        };

        let airdrop_sig = match self
            .rpc_client
            .request_airdrop_with_config(&pubkey, lamports, config.unwrap_or_default())
            .await
        {
            Ok(airdrop_sig) => airdrop_sig.to_string(),
            Err(err) => {
                return Err(jsonrpsee::core::Error::Custom(err.to_string()));
            }
        };

        self.tx_sender
            .txs_sent_store
            .insert(airdrop_sig.clone(), Default::default());

        Ok(airdrop_sig)
    }

    fn signature_subscribe(
        &self,
        mut sink: SubscriptionSink,
        signature: String,
        commitment_config: CommitmentConfig,
    ) -> SubscriptionResult {
        RPC_SIGNATURE_SUBSCRIBE.inc();
        sink.accept()?;
        self.block_listner
            .signature_subscribe(signature, commitment_config, sink);
        Ok(())
    }
}

impl Deref for LiteBridge {
    type Target = RpcClient;

    fn deref(&self) -> &Self::Target {
        &self.rpc_client
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiteRpcResponseContext {
    pub slot: Slot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_version: Option<RpcApiVersion>,
    pub sampled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiteResponse<T> {
    pub context: LiteRpcResponseContext,
    pub value: T,
}
