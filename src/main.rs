use alloy_consensus::TxEnvelope;
use alloy_network::{BlockResponse, Ethereum, Network};
use alloy_primitives::{Address, B256, U256, map::HashMap};
use alloy_provider::{Provider, RootProvider, network::primitives::HeaderResponse};
use alloy_rpc_client::RpcClient;
use alloy_transport::{
    TransportError,
    layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy},
};
use core::fmt::Debug;
use reth_chainspec::{ChainSpec, MAINNET};
use reth_evm::execute::{BasicBlockExecutor, Executor};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::Block as BlockTrait;
use reth_storage_errors::provider::ProviderError;
use revm_database::CacheDB;
use revm_database_interface::DatabaseRef;
use revm_state::{AccountInfo, Bytecode};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    fs::File,
    io::{BufReader, BufWriter},
    path::Path,
    sync::Arc,
};
use tracing::debug;
use url::Url;

// failed blocks
// const TEST_BLOCK_NUMBER: u64 = 23139688;
// const TEST_BLOCK_NUMBER: u64 = 23044446;
const TEST_BLOCK_NUMBER: u64 = 23043259;

// set to a valid rpc url
const RPC_URL: &str = "RPC_URL_TO_SET";

// local serialized RPC DB
const LOCAL_RPC_DB_PATH: &str = "rpc_db.bin";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    let rpc_url = Url::parse(RPC_URL).unwrap();
    let provider = create_provider(rpc_url);

    EthHostExecutor::eth(MAINNET.clone())
        .execute(TEST_BLOCK_NUMBER, &provider)
        .await;

    Ok(())
}

#[derive(Debug, Copy, Clone, Default)]
struct ServerErrorRetryPolicy(RateLimitRetryPolicy);

impl RetryPolicy for ServerErrorRetryPolicy {
    fn should_retry(&self, error: &TransportError) -> bool {
        if self.0.should_retry(error) {
            return true;
        }

        false
    }

    fn backoff_hint(&self, error: &TransportError) -> Option<std::time::Duration> {
        self.0.backoff_hint(error)
    }
}

fn create_provider<N: Network>(rpc_url: Url) -> RootProvider<N> {
    let retry_layer =
        RetryBackoffLayer::new_with_policy(3, 1000, 100, ServerErrorRetryPolicy::default());
    let client = RpcClient::builder().layer(retry_layer).http(rpc_url);

    RootProvider::new(client)
}

#[derive(Debug, Clone)]
pub struct EthHostExecutor {
    evm_config: EthEvmConfig,
}

impl EthHostExecutor {
    pub fn eth(chain_spec: Arc<ChainSpec>) -> Self {
        tracing::info!("chain_spec: {chain_spec:#?}");
        Self {
            evm_config: EthEvmConfig::new(chain_spec),
        }
    }
}

impl EthHostExecutor {
    pub async fn execute<P>(&self, block_number: u64, provider: &P)
    where
        P: Provider<Ethereum> + Clone + std::fmt::Debug,
    {
        tracing::info!("fetching the current block and the previous block");
        let current_block = provider
            .get_block_by_number(block_number.into())
            .full()
            .await
            .unwrap()
            .unwrap();
        let current_block = current_block.map_transactions(|tx| TxEnvelope::from(tx).into());
        let current_block = current_block.into_consensus();

        tracing::info!("setting up the database for the block executor");
        let rpc_db = RpcDb::new(provider.clone(), block_number - 1);

        let cache_db = CacheDB::new(&rpc_db);

        let block_executor = BasicBlockExecutor::new(self.evm_config.clone(), cache_db);
        let block = current_block.clone().try_into_recovered().unwrap();

        let execution_output = block_executor.execute(&block).unwrap();
        {
            let file = File::create(LOCAL_RPC_DB_PATH).unwrap();
            let writer = BufWriter::new(file);
            bincode::serialize_into(writer, &rpc_db.data).unwrap();
        }

        tracing::info!("validating the block post execution");
        reth_ethereum_consensus::validate_block_post_execution(
            &block,
            &self.evm_config.chain_spec().clone(),
            &execution_output.result.receipts,
            &execution_output.result.requests,
        )
        .unwrap();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcDbData {
    /// The cached accounts.
    pub accounts: HashMap<Address, AccountInfo>,
    /// The cached storage values.
    pub storage: HashMap<Address, HashMap<U256, U256>>,
}

#[derive(Debug, Clone)]
pub struct RpcDb<P> {
    /// The provider which fetches data.
    pub provider: P,
    /// The block to fetch data from.
    pub block_number: u64,
    /// cached db data
    pub data: RefCell<RpcDbData>,
}

impl<P: Provider<Ethereum> + Clone> RpcDb<P> {
    pub fn new(provider: P, block_number: u64) -> Self {
        let path = Path::new(LOCAL_RPC_DB_PATH);
        let data = if path.exists() {
            let file = File::open(path).unwrap();
            let reader = BufReader::new(file);
            bincode::deserialize_from(reader).unwrap()
        } else {
            RpcDbData {
                accounts: HashMap::with_hasher(Default::default()),
                storage: HashMap::with_hasher(Default::default()),
            }
        };

        Self {
            provider,
            block_number,
            data: RefCell::new(data),
        }
    }

    pub async fn fetch_account_info(&self, address: Address) -> Result<AccountInfo, ()> {
        debug!("fetching account info for address: {}", address);

        if self.data.borrow().accounts.contains_key(&address) {
            let acc = self.data.borrow().accounts.get(&address).unwrap().clone();
            if acc.is_empty() {
                println!("gupeng - empty account");
            }
            return Ok(acc);
        }

        let proof = self
            .provider
            .get_proof(address, vec![])
            .number(self.block_number)
            .await
            .unwrap();

        // Fetch the code of the account.
        let code = self
            .provider
            .get_code_at(address)
            .number(self.block_number)
            .await
            .unwrap();

        // Construct the account info & write it to the log.
        let bytecode = Bytecode::new_raw(code);
        let account_info = AccountInfo {
            nonce: proof.nonce,
            balance: proof.balance,
            code_hash: proof.code_hash,
            code: Some(bytecode.clone()),
        };

        // Record the account info to the state.
        self.data
            .borrow_mut()
            .accounts
            .insert(address, account_info.clone());

        Ok(account_info)
    }

    pub async fn fetch_storage_at(&self, address: Address, index: U256) -> Result<U256, ()> {
        debug!(
            "fetching storage value at address: {}, index: {}",
            address, index,
        );

        if let Some(storage_map) = self.data.borrow().storage.get(&address)
            && let Some(value) = storage_map.get(&index)
        {
            return Ok(*value);
        }

        let value = self
            .provider
            .get_storage_at(address, index)
            .number(self.block_number)
            .await
            .unwrap();

        // Record the storage value to the state.
        let storage_values = &mut self.data.borrow_mut().storage;
        let entry = storage_values.entry(address).or_default();
        entry.insert(index, value);

        Ok(value)
    }

    pub async fn fetch_block_hash(&self, number: u64) -> Result<B256, ()> {
        debug!("fetching block hash for block number: {}", number);

        // Fetch the block.
        let block = self
            .provider
            .get_block_by_number(number.into())
            .await
            .unwrap();

        // Record the block hash to the state.
        let block = block.unwrap();
        let hash = block.header().hash();

        Ok(hash)
    }
}

impl<P: Provider<Ethereum> + Clone> DatabaseRef for RpcDb<P> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let handle = tokio::runtime::Handle::current();
        let result =
            tokio::task::block_in_place(|| handle.block_on(self.fetch_account_info(address)));
        let account = result.unwrap();
        Ok(Some(account))
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        unimplemented!()
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let handle = tokio::runtime::Handle::current();
        let result =
            tokio::task::block_in_place(|| handle.block_on(self.fetch_storage_at(address, index)));
        let value = result.unwrap();
        Ok(value)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::block_in_place(|| handle.block_on(self.fetch_block_hash(number)));
        let value = result.unwrap();
        Ok(value)
    }
}
