use alloy_chains::Chain;
use alloy_consensus::{Block, BlockHeader, TxEnvelope};
use alloy_evm::EthEvm;
use alloy_network::{BlockResponse, Ethereum, Network};
use alloy_primitives::{Address, B256, U256, map::HashMap};
use alloy_provider::{Provider, RootProvider, network::primitives::HeaderResponse};
use alloy_rpc_client::RpcClient;
use alloy_transport::{
    TransportError,
    layers::{RateLimitRetryPolicy, RetryBackoffLayer, RetryPolicy},
};
use core::fmt::Debug;
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, ChainSpec, EthereumHardfork};
use reth_errors::ConsensusError;
use reth_ethereum_primitives::EthPrimitives;
use reth_evm::{
    ConfigureEvm, Database, EvmEnv, EvmFactory,
    execute::{BasicBlockExecutor, Executor},
    precompiles::PrecompilesMap,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_execution_types::BlockExecutionOutput;
use reth_primitives_traits::{Block as BlockTrait, NodePrimitives, RecoveredBlock};
use reth_storage_errors::{db::DatabaseError, provider::ProviderError};
use revm::{
    Context, MainBuilder, MainContext,
    context::{
        BlockEnv, CfgEnv, TxEnv,
        result::{EVMError, HaltReason},
    },
    database::CacheDB,
    handler::EthPrecompiles,
    inspector::NoOpInspector,
};
use revm_database_interface::DatabaseRef;
use revm_primitives::hardfork::SpecId;
use revm_state::{AccountInfo, Bytecode};
use std::{
    marker::PhantomData,
    sync::{Arc, RwLock},
};
use tracing::debug;
use url::Url;

const TEST_BLOCK_NUMBER: u64 = 23_139_688;

// set to a valid rpc url
const RPC_URL: &str = "SET_TO_VALID_RPC_URL";

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    let rpc_url = Url::parse(RPC_URL).unwrap();
    let provider = create_provider(rpc_url);

    EthHostExecutor::eth(Arc::new(mainnet()))
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

fn mainnet() -> ChainSpec {
    ChainSpec {
        chain: Chain::mainnet(),
        genesis: Default::default(),
        genesis_header: Default::default(),
        paris_block_and_final_difficulty: Default::default(),
        hardforks: EthereumHardfork::mainnet().into(),
        deposit_contract: Default::default(),
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::ethereum()),
        prune_delete_limit: 20000,
        blob_params: Default::default(),
    }
}

#[derive(Debug, Clone)]
pub struct CustomEvmFactory;

impl EvmFactory for CustomEvmFactory {
    type Evm<DB: Database, I: revm::Inspector<Self::Context<DB>>> = EthEvm<DB, I, PrecompilesMap>;

    type Context<DB: Database> = Context<BlockEnv, TxEnv, CfgEnv, DB>;

    type Tx = TxEnv;

    type Error<DBError: std::error::Error + Send + Sync + 'static> = EVMError<DBError>;

    type HaltReason = HaltReason;

    type Spec = SpecId;

    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        mut input: EvmEnv,
    ) -> Self::Evm<DB, revm::inspector::NoOpInspector> {
        #[allow(unused_mut)]
        let mut precompiles = PrecompilesMap::from(EthPrecompiles::default());

        let evm = Context::mainnet()
            .with_db(db)
            .with_cfg(input.cfg_env)
            .with_block(input.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(precompiles);

        EthEvm::new(evm, false)
    }

    fn create_evm_with_inspector<DB: Database, I: revm::Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        mut input: EvmEnv,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        EthEvm::new(
            self.create_evm(db, input)
                .into_inner()
                .with_inspector(inspector),
            true,
        )
    }
}

fn create_provider<N: Network>(rpc_url: Url) -> RootProvider<N> {
    let retry_layer =
        RetryBackoffLayer::new_with_policy(3, 1000, 100, ServerErrorRetryPolicy::default());
    let client = RpcClient::builder().layer(retry_layer).http(rpc_url);

    RootProvider::new(client)
}

pub type EthHostExecutor = HostExecutor<EthEvmConfig<ChainSpec, CustomEvmFactory>, ChainSpec>;

/// An executor that fetches data from a [Provider] to execute blocks in the [ClientExecutor].
#[derive(Debug, Clone)]
pub struct HostExecutor<C: ConfigureEvm, CS> {
    evm_config: C,
    chain_spec: Arc<CS>,
}

impl EthHostExecutor {
    pub fn eth(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            evm_config: EthEvmConfig::new_with_evm_factory(chain_spec.clone(), CustomEvmFactory),
            chain_spec,
        }
    }
}

impl<C: ConfigureEvm, CS> HostExecutor<C, CS> {
    pub async fn execute<P, N>(&self, block_number: u64, provider: &P)
    where
        C::Primitives: IntoPrimitives<N> + IntoInput + BlockValidator<CS>,
        P: Provider<N> + Clone + std::fmt::Debug,
        N: Network,
    {
        // Fetch the current block and the previous block from the provider.
        tracing::info!("fetching the current block and the previous block");
        let rpc_block = provider
            .get_block_by_number(block_number.into())
            .full()
            .await
            .unwrap()
            .unwrap();

        let current_block = C::Primitives::into_primitive_block(rpc_block.clone());

        let previous_block = provider
            .get_block_by_number((block_number - 1).into())
            .full()
            .await
            .unwrap()
            .map(C::Primitives::into_primitive_block)
            .unwrap();

        // Setup the database for the block executor.
        tracing::info!("setting up the database for the block executor");
        let rpc_db = RpcDb::new(
            provider.clone(),
            block_number - 1,
            previous_block.header().state_root(),
        );

        let cache_db = CacheDB::new(&rpc_db);

        let block_executor = BasicBlockExecutor::new(self.evm_config.clone(), cache_db);
        let block = current_block.clone().try_into_recovered().unwrap();

        let execution_output = block_executor.execute(&block).unwrap();

        // Validate the block post execution.
        tracing::info!("validating the block post execution");
        C::Primitives::validate_block_post_execution(
            &block,
            self.chain_spec.clone(),
            &execution_output,
        )
        .unwrap();
    }
}

pub trait IntoPrimitives<N: Network>: NodePrimitives {
    fn into_primitive_block(block: N::BlockResponse) -> Self::Block;
}

pub trait IntoInput: NodePrimitives {
    fn into_input_block(block: Self::Block) -> Block<Self::SignedTx>;
}

pub trait BlockValidator<CS>: NodePrimitives {
    fn validate_block_post_execution(
        block: &RecoveredBlock<Self::Block>,
        chain_spec: Arc<CS>,
        execution_output: &BlockExecutionOutput<Self::Receipt>,
    ) -> Result<(), ConsensusError>;
}

impl IntoPrimitives<Ethereum> for EthPrimitives {
    fn into_primitive_block(block: alloy_rpc_types::Block) -> Self::Block {
        let block = block.map_transactions(|tx| TxEnvelope::from(tx).into());
        block.into_consensus()
    }
}

impl IntoInput for EthPrimitives {
    fn into_input_block(block: Self::Block) -> Block<Self::SignedTx> {
        block
    }
}

impl BlockValidator<ChainSpec> for EthPrimitives {
    fn validate_block_post_execution(
        block: &RecoveredBlock<Self::Block>,
        chain_spec: Arc<ChainSpec>,
        execution_output: &BlockExecutionOutput<Self::Receipt>,
    ) -> Result<(), ConsensusError> {
        reth_ethereum_consensus::validate_block_post_execution(
            block,
            &chain_spec,
            &execution_output.result.receipts,
            &execution_output.result.requests,
        )
    }
}

#[derive(Debug, Clone)]
pub struct RpcDb<P, N> {
    /// The provider which fetches data.
    pub provider: P,
    /// The block to fetch data from.
    pub block_number: u64,
    ///The state root to fetch data from.
    pub state_root: B256,
    /// The cached accounts.
    pub accounts: Arc<RwLock<HashMap<Address, AccountInfo>>>,
    /// The cached storage values.
    pub storage: Arc<RwLock<HashMap<Address, HashMap<U256, U256>>>>,
    /// The oldest block whose header/hash has been requested.
    pub oldest_ancestor: Arc<RwLock<u64>>,

    phantom: PhantomData<N>,
}

impl<P: Provider<N> + Clone, N: Network> RpcDb<P, N> {
    pub fn new(provider: P, block_number: u64, state_root: B256) -> Self {
        Self {
            provider,
            block_number,
            state_root,
            accounts: Arc::new(RwLock::new(HashMap::with_hasher(Default::default()))),
            storage: Arc::new(RwLock::new(HashMap::with_hasher(Default::default()))),
            oldest_ancestor: Arc::new(RwLock::new(block_number)),
            phantom: PhantomData,
        }
    }

    pub async fn fetch_account_info(&self, address: Address) -> Result<AccountInfo, ()> {
        debug!("fetching account info for address: {}", address);

        // Fetch the proof for the account.
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
        self.accounts
            .write()
            .unwrap()
            .insert(address, account_info.clone());

        Ok(account_info)
    }

    pub async fn fetch_storage_at(&self, address: Address, index: U256) -> Result<U256, ()> {
        debug!(
            "fetching storage value at address: {}, index: {}",
            address, index
        );

        // Fetch the storage value.
        let value = self
            .provider
            .get_storage_at(address, index)
            .number(self.block_number)
            .await
            .unwrap();

        // Record the storage value to the state.
        let mut storage_values = self.storage.write().unwrap();
        let entry = storage_values.entry(address).or_default();
        entry.insert(index, value);

        Ok(value)
    }

    /// Fetch the block hash for a block number.
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

        let mut oldest_ancestor = self.oldest_ancestor.write().unwrap();
        *oldest_ancestor = number.min(*oldest_ancestor);

        Ok(hash)
    }
}

impl<P: Provider<N> + Clone, N: Network> DatabaseRef for RpcDb<P, N> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            ProviderError::Database(DatabaseError::Other("no tokio runtime found".to_string()))
        })?;
        let result =
            tokio::task::block_in_place(|| handle.block_on(self.fetch_account_info(address)));
        let account_info = result.unwrap();
        Ok((!account_info.is_empty()).then_some(account_info))
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        unimplemented!()
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            ProviderError::Database(DatabaseError::Other("no tokio runtime found".to_string()))
        })?;
        let result =
            tokio::task::block_in_place(|| handle.block_on(self.fetch_storage_at(address, index)));
        let value = result.unwrap();
        Ok(value)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            ProviderError::Database(DatabaseError::Other("no tokio runtime found".to_string()))
        })?;
        let result = tokio::task::block_in_place(|| handle.block_on(self.fetch_block_hash(number)));
        let value = result.unwrap();
        Ok(value)
    }
}
