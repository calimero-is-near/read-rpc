use crate::modules::blocks::CacheBlock;
use clap::Parser;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
pub struct Opts {
    // near network rpc url
    #[clap(long, env = "NEAR_RPC_URL")]
    pub rpc_url: http::Uri,
    // near network rpc url
    #[clap(long, env = "NEAR_RPC_API_KEY")]
    pub rpc_api_key: Option<String>,

    // Indexer bucket name
    #[clap(long, env = "AWS_BUCKET_NAME")]
    pub s3_bucket_name: String,

    // Scylla db url
    #[clap(long, env)]
    pub scylla_url: String,

    /// ScyllaDB user(login)
    #[clap(long, env)]
    pub scylla_user: Option<String>,

    /// ScyllaDB password
    #[clap(long, env)]
    pub scylla_password: Option<String>,

    /// ScyllaDB preferred DataCenter
    /// Accepts the DC name of the ScyllaDB to filter the connection to that DC only (preferrably).
    /// If you connect to multi-DC cluter, you might experience big latencies while working with the DB. This is due to the fact that ScyllaDB driver tries to connect to any of the nodes in the cluster disregarding of the location of the DC. This option allows to filter the connection to the DC you need. Example: "DC1" where DC1 is located in the same region as the application.
    #[clap(long, env)]
    pub scylla_preferred_dc: Option<String>,

    /// ScyllaDB keepalive interval
    #[clap(long, env, default_value = "60")]
    pub scylla_keepalive_interval: u64,

    // AWS endpoint
    #[clap(long, env = "AWS_ENDPOINT")]
    pub endpoint: String,

    // AWS access key id
    #[clap(long, env = "AWS_ACCESS_KEY_ID")]
    pub access_key_id: String,

    // AWS secret access key
    #[clap(long, env = "AWS_SECRET_ACCESS_KEY")]
    pub secret_access_key: String,

    // AWS default region
    #[clap(long, env = "AWS_DEFAULT_REGION")]
    pub region: String,

    // AWS default region
    #[clap(long, env, default_value = "8000")]
    pub server_port: u16,

    /// Max retry count for ScyllaDB if `strict_mode` is `false`
    #[clap(long, default_value = "2", env)]
    pub max_retry: u8,

    /// Attempts to store data in the database should be infinite to ensure no data is missing.
    /// Disable it to perform a limited write attempts (`max_retry`)
    /// before skipping giving up and moving to the next piece of data
    #[clap(long, default_value = "false", env)]
    pub strict_mode: bool,

    /// Max gas burnt for contract function call
    /// Default value is 300_000_000_000_000
    #[clap(long, env, default_value = "300000000000000")]
    pub max_gas_burnt: near_primitives_core::types::Gas,

    /// Max available memory for `block_cache` and `contract_code_cache` in gigabytes
    /// By default we use all available memory
    #[clap(long, env)]
    pub limit_memory_cache: Option<f64>,

    /// Reserved memory for running the application in gigabytes
    /// By default we use 0.25 gigabyte (256MB or 268_435_456 bytes)
    #[clap(long, env, default_value = "0.25")]
    pub reserved_memory: f64,

    /// Block cache size in gigabytes
    /// By default we use 0.125 gigabyte (128MB or 134_217_728 bytes)
    /// One cache_block size is ≈ 96 bytes
    /// In 128MB we can put 1_398_101 cache_blocks
    #[clap(long, env, default_value = "0.125")]
    pub block_cache_size: f64,
}

impl Opts {
    pub async fn to_s3_config(&self) -> aws_sdk_s3::Config {
        let credentials = aws_credential_types::Credentials::new(
            &self.access_key_id,
            &self.secret_access_key,
            None,
            None,
            "",
        );
        aws_sdk_s3::Config::builder()
            .credentials_provider(credentials)
            .region(aws_sdk_s3::Region::new(self.region.clone()))
            .endpoint_url(&self.endpoint)
            .build()
    }

    pub async fn to_lake_config(
        &self,
        start_block_height: near_primitives_core::types::BlockHeight,
    ) -> anyhow::Result<near_lake_framework::LakeConfig> {
        let config_builder = near_lake_framework::LakeConfigBuilder::default();
        Ok(config_builder
            .s3_config(self.to_s3_config().await)
            .s3_region_name(&self.region)
            .s3_bucket_name(&self.s3_bucket_name)
            .start_block_height(start_block_height)
            .build()
            .expect("Failed to build LakeConfig"))
    }
}

pub struct ServerContext {
    pub s3_client: near_lake_framework::s3_fetchers::LakeS3Client,
    pub scylla_db_manager: std::sync::Arc<crate::storage::ScyllaDBManager>,
    pub near_rpc_client: near_jsonrpc_client::JsonRpcClient,
    pub s3_bucket_name: String,
    pub genesis_config: near_chain_configs::GenesisConfig,
    pub blocks_cache:
        std::sync::Arc<std::sync::RwLock<crate::cache::LruMemoryCache<u64, CacheBlock>>>,
    pub final_block_height: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub compiled_contract_code_cache: std::sync::Arc<CompiledCodeCache>,
    pub contract_code_cache: std::sync::Arc<
        std::sync::RwLock<crate::cache::LruMemoryCache<near_primitives::hash::CryptoHash, Vec<u8>>>,
    >,
    pub max_gas_burnt: near_primitives_core::types::Gas,
}

pub struct CompiledCodeCache {
    pub local_cache: std::sync::Arc<
        std::sync::RwLock<
            crate::cache::LruMemoryCache<
                near_primitives::hash::CryptoHash,
                near_primitives::types::CompiledContract,
            >,
        >,
    >,
}

impl near_primitives::types::CompiledContractCache for CompiledCodeCache {
    fn put(
        &self,
        key: &near_primitives::hash::CryptoHash,
        value: near_primitives::types::CompiledContract,
    ) -> std::io::Result<()> {
        self.local_cache.write().unwrap().put(*key, value);
        Ok(())
    }

    fn get(
        &self,
        key: &near_primitives::hash::CryptoHash,
    ) -> std::io::Result<Option<near_primitives::types::CompiledContract>> {
        Ok(self.local_cache.write().unwrap().get(key).cloned())
    }

    fn has(&self, key: &near_primitives::hash::CryptoHash) -> std::io::Result<bool> {
        Ok(self.local_cache.write().unwrap().contains(key))
    }
}
