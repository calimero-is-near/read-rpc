use crate::utils::{
    get_final_cache_block, gigabytes_to_bytes, update_final_block_height_regularly,
};
use clap::Parser;
use config::{Opts, ServerContext};
use database::ScyllaStorageManager;
use dotenv::dotenv;
use jsonrpc_v2::{Data, Server};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[macro_use]
extern crate lazy_static;

mod cache;
mod config;
mod errors;
mod metrics;
mod modules;
mod storage;
mod utils;

fn init_logging(use_tracer: bool) -> anyhow::Result<()> {
    // Filter based on level - trace, debug, info, warn, error
    // Tunable via `RUST_LOG` env variable
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or(tracing_subscriber::EnvFilter::new("info"));

    // Combined them all together in a `tracing` subscriber
    let subscriber = tracing_subscriber::Registry::default().with(env_filter);

    if use_tracer {
        let app_name = "read_rpc_server";
        // Start a new Jaeger trace pipeline.
        // Spans are exported in batch - recommended setup for a production application.
        opentelemetry::global::set_text_map_propagator(
            opentelemetry::sdk::propagation::TraceContextPropagator::new(),
        );
        let tracer = opentelemetry_jaeger::new_pipeline()
            .with_service_name(app_name)
            .install_simple()?;
        // Create a `tracing` layer using the Jaeger tracer
        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

        if std::env::var("ENABLE_JSON_LOGS").is_ok() {
            subscriber
                .with(telemetry)
                .with(tracing_subscriber::fmt::Layer::default().json())
                .try_init()?;
        } else {
            subscriber
                .with(telemetry)
                .with(tracing_subscriber::fmt::Layer::default().compact())
                .try_init()?;
        };
    } else if std::env::var("ENABLE_JSON_LOGS").is_ok() {
        subscriber.with(tracing_stackdriver::layer()).try_init()?;
    } else {
        subscriber
            .with(tracing_subscriber::fmt::Layer::default().compact())
            .try_init()?;
    };

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();

    let opts: Opts = Opts::parse();

    #[cfg(feature = "tracing-instrumentation")]
    init_logging(true)?;

    #[cfg(not(feature = "tracing-instrumentation"))]
    init_logging(false)?;

    let near_rpc_client = near_jsonrpc_client::JsonRpcClient::connect(opts.rpc_url.to_string());
    // We want to set a custom referer to let NEAR JSON RPC nodes know that we are a read-rpc instance
    let mut near_rpc_client = near_rpc_client.header(("Referer", "read-rpc"))?; // TODO: make it configurable
    if let Some(key) = &opts.rpc_api_key {
        near_rpc_client = near_rpc_client.header(("x-api-key", key))?;
    }

    let final_block = get_final_cache_block(&near_rpc_client)
        .await
        .expect("Error to get final block");

    let limit_memory_cache_in_bytes = if let Some(limit_memory_cache) = opts.limit_memory_cache {
        Some(gigabytes_to_bytes(limit_memory_cache).await)
    } else {
        None
    };
    let reserved_memory_in_bytes = gigabytes_to_bytes(opts.reserved_memory).await;
    let block_cache_size_in_bytes = gigabytes_to_bytes(opts.block_cache_size).await;

    let contract_code_cache_size = utils::calculate_contract_code_cache_sizes(
        reserved_memory_in_bytes,
        block_cache_size_in_bytes,
        limit_memory_cache_in_bytes,
    )
    .await;

    let blocks_cache = std::sync::Arc::new(std::sync::RwLock::new(cache::LruMemoryCache::new(
        block_cache_size_in_bytes,
    )));

    let final_block_height =
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(final_block.block_height));
    blocks_cache
        .write()
        .unwrap()
        .put(final_block.block_height, final_block);

    let compiled_contract_code_cache = std::sync::Arc::new(config::CompiledCodeCache {
        local_cache: std::sync::Arc::new(std::sync::RwLock::new(cache::LruMemoryCache::new(
            contract_code_cache_size,
        ))),
    });
    let contract_code_cache = std::sync::Arc::new(std::sync::RwLock::new(
        cache::LruMemoryCache::new(contract_code_cache_size),
    ));

    let scylla_db_manager = std::sync::Arc::new(
        *storage::ScyllaDBManager::new(
            &opts.scylla_url,
            opts.scylla_user.as_deref(),
            opts.scylla_password.as_deref(),
            opts.scylla_preferred_dc.as_deref(),
            Some(opts.scylla_keepalive_interval),
            opts.max_retry,
            opts.strict_mode,
        )
        .await?,
    );

    tracing::info!("Get genesis config...");
    let genesis_config = near_rpc_client
        .call(near_jsonrpc_client::methods::EXPERIMENTAL_genesis_config::RpcGenesisConfigRequest)
        .await?;
    let lake_config = opts.to_lake_config(final_block.block_height).await?;
    let s3_config = opts.to_s3_config().await;

    let state = ServerContext {
        s3_client: near_lake_framework::s3_fetchers::LakeS3Client::new(
            aws_sdk_s3::Client::from_conf(s3_config),
        ),
        scylla_db_manager,
        near_rpc_client: near_rpc_client.clone(),
        s3_bucket_name: opts.s3_bucket_name,
        genesis_config,
        blocks_cache: std::sync::Arc::clone(&blocks_cache),
        final_block_height: std::sync::Arc::clone(&final_block_height),
        compiled_contract_code_cache,
        contract_code_cache,
        max_gas_burnt: opts.max_gas_burnt,
    };

    tokio::spawn(async move {
        update_final_block_height_regularly(final_block_height.clone(), blocks_cache, lake_config)
            .await
    });

    let rpc = Server::new()
        .with_data(Data::new(state))
        .with_method("query", modules::queries::methods::query)
        .with_method("block", modules::blocks::methods::block)
        .with_method(
            "EXPERIMENTAL_changes",
            modules::blocks::methods::changes_in_block_by_type,
        )
        .with_method(
            "EXPERIMENTAL_changes_in_block",
            modules::blocks::methods::changes_in_block,
        )
        .with_method("chunk", modules::blocks::methods::chunk)
        .with_method("tx", modules::transactions::methods::tx)
        .with_method(
            "EXPERIMENTAL_tx_status",
            modules::transactions::methods::tx_status,
        )
        .with_method(
            "broadcast_tx_async",
            modules::transactions::methods::send_tx_async,
        )
        .with_method(
            "broadcast_tx_commit",
            modules::transactions::methods::send_tx_commit,
        )
        .with_method("gas_price", modules::gas::methods::gas_price)
        .with_method("status", modules::network::methods::status)
        .with_method(
            "light_client_proof",
            modules::clients::methods::light_client_proof,
        )
        .with_method(
            "next_light_client_block",
            modules::clients::methods::next_light_client_block,
        )
        .with_method("network_info", modules::network::methods::network_info)
        .with_method("validators", modules::network::methods::validators)
        .with_method(
            "EXPERIMENTAL_validators_ordered",
            modules::network::methods::validators_ordered,
        )
        .with_method(
            "EXPERIMENTAL_genesis_config",
            modules::network::methods::genesis_config,
        )
        .with_method(
            "EXPERIMENTAL_protocol_config",
            modules::network::methods::protocol_config,
        )
        .with_method("EXPERIMENTAL_receipt", modules::receipts::methods::receipt)
        .finish();

    actix_web::HttpServer::new(move || {
        let rpc = rpc.clone();
        actix_web::App::new()
            .wrap(tracing_actix_web::TracingLogger::default())
            .service(
                actix_web::web::service("/")
                    .guard(actix_web::guard::Post())
                    .finish(rpc.into_web_service()),
            )
            .service(metrics::get_metrics)
    })
    .bind(format!("0.0.0.0:{:0>5}", opts.server_port))?
    .run()
    .await?;

    Ok(())
}
