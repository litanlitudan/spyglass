use jsonrpc_core_client::{transports::ipc, TypedClient};
use shared::rpc::gen_ipc_path;
use tokio_retry::Retry;
use tokio_retry::strategy::{ExponentialBackoff, jitter};

pub struct RpcClient {
    pub client: TypedClient,
    pub endpoint: String,
}

async fn connect(endpoint: String) -> Result<TypedClient, ()> {
    if let Ok(client) = ipc::connect(endpoint.clone()).await {
        return Ok(client);
    }

    Err(())
}

impl RpcClient {
    pub async fn new() -> Self {
        let endpoint = gen_ipc_path();

        let retry_strategy = ExponentialBackoff::from_millis(10)
            .map(jitter) // add jitter to delays
            .take(3);


        let client: TypedClient = Retry::spawn(
            retry_strategy,
            || { connect(endpoint.clone()) }
        ).await.unwrap();

        RpcClient {
            client,
            endpoint: endpoint.clone(),
        }
    }
}