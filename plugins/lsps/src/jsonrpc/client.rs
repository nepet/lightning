use async_trait::async_trait;
use log::{debug, trace};
use rand::rngs::OsRng;
use rand::TryRngCore;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

use crate::jsonrpc::messages::{JsonRpcRequest, RequestObject, ResponseObject, RpcError};

#[async_trait]
pub trait AsyncTransport: Send + Sync + 'static {
    type Error: core::fmt::Display;
    async fn send_message(&self, message: String) -> Result<(), Self::Error>;
    async fn receive_message(&self) -> Option<Result<String, Self::Error>>;
}

#[derive(Clone)]
pub struct JsonRpcClient<T: AsyncTransport> {
    transport: Arc<T>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl<T: AsyncTransport> JsonRpcClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn call_method(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = generate_random_id();
        let request = RequestObject {
            jsonrpc: "2.0",
            method,
            params,
            id: Some(id.clone().into()),
        };

        let (tx, rx) = oneshot::channel();
        let mut requests = self.pending_requests.lock().await;
        requests.insert(id.clone(), tx);
        drop(requests);

        self.transport
            .send_message(serde_json::to_string(&request).map_err(|e| RpcError {
                code: 0,
                message: e.to_string().into(),
                data: None,
            })?)
            .await
            .map_err(|e| RpcError::internal_error(e, None))?;

        let response = match rx.await {
            Ok(response) => response,
            Err(_) => {
                return Err(RpcError {
                    code: -32603,
                    message: "channel closed unexpectedly".into(),
                    data: None,
                })
            }
        };

        serde_json::from_str(&response).map_err(|e| RpcError {
            code: -32603,
            message: format!("could not deserialize response: {response}: {e}").into(),
            data: None,
        })
    }

    pub async fn call_typed<'a, RQ, RS>(&self, request: RQ) -> Result<RS, RpcError>
    where
        RQ: JsonRpcRequest<'a> + Serialize + Send + Sync,
        RS: DeserializeOwned + Serialize + Send + Sync,
    {
        let id = generate_random_id();
        let request = request.into_request(Some(id.clone().into()));
        let request_json = serde_json::to_string(&request).map_err(|_| RpcError {
            code: 0,
            message: "err".into(),
            data: None,
        })?;

        let (tx, rx) = oneshot::channel();
        let mut requests = self.pending_requests.lock().await;
        trace!("Acquired lock");
        requests.insert(id.clone(), tx);
        drop(requests);
        trace!("Lock released");

        self.transport
            .send_message(request_json)
            .await
            .map_err(|e| RpcError::internal_error(e, None))?;
        trace!("Message with id {id} was sent to transport, waiting for response.");
        let response = match rx.await {
            Ok(response) => response,
            Err(_) => {
                return Err(RpcError {
                    code: -32603,
                    message: "channel closed unexpectedly".into(),
                    data: None,
                })
            }
        };

        let response_obj: ResponseObject<RS> =
            serde_json::from_str(&response).map_err(|e| RpcError {
                code: -32603,
                message: format!("could not deserialize response: {response}: {e}").into(),
                data: None,
            })?;

        Ok(response_obj.into_inner()?)
    }

    async fn handle_response(&self, response: String) {
        trace!("Handle response: {response}");
        let value: serde_json::Value = match serde_json::from_str(&response) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to deserialize response string: {e}");
                return;
            }
        };

        let id = match value.get("id").and_then(|id| id.as_str()) {
            Some(id) => id.to_string(),
            None => {
                trace!("No id in server response, skip handling response");
                return;
            }
        };

        let mut requests = self.pending_requests.lock().await;
        trace!("Acquired lock");
        if let Some(sender) = requests.remove(&id) {
            match sender.send(response) {
                Ok(_) => (),
                Err(e) => debug!("Failed to pass response to oneshot channel: {e}"),
            };
        } else {
            debug!("Could not find a pending request for response with id {id}");
        }
        trace!("Release lock");
    }

    pub async fn listen(&self) {
        trace!("listening for incoming messages");
        while let Some(result) = self.transport.receive_message().await {
            match result {
                Ok(val) => self.handle_response(val).await,
                Err(e) => {
                    debug!("received an error from transport: {e}");
                }
            };
        }
        trace!("stop listening for incoming messages");
    }
}

fn generate_random_id() -> String {
    let mut bytes = [0u8; 10];
    OsRng.try_fill_bytes(&mut bytes).unwrap();
    hex::encode(bytes)
}

#[cfg(test)]

mod test_json_rpc {
    use serde::Deserialize;
    use tokio::{
        join,
        sync::Mutex,
        time::{self, Duration},
    };

    use super::*;
    use crate::jsonrpc::messages::JsonRpcResponse;

    #[derive(Clone)]
    struct TestTransport {
        req: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl AsyncTransport for TestTransport {
        type Error = RpcError;
        async fn send_message(&self, message: String) -> Result<(), Self::Error> {
            let mut id = self.req.lock().await;
            *id = Some(message);
            Ok(())
        }
        async fn receive_message(&self) -> Option<Result<String, Self::Error>> {
            Some(Ok(String::default()))
        }
    }

    #[derive(Default, Clone, Serialize, Deserialize)]
    struct DummyCall {
        foo: String,
        bar: i32,
    }

    impl<'a> JsonRpcRequest<'a> for DummyCall {
        fn method_name() -> &'static str {
            "dummy_call"
        }
    }

    #[derive(Serialize, Deserialize, Clone, Debug, Default)]
    struct DummyResponse {
        foo: String,
        bar: i32,
    }

    #[tokio::test]
    async fn test_typed_call_w_response() {
        let req = DummyCall {
            foo: "hello world!".into(),
            bar: 13,
        };

        let transport = TestTransport {
            req: Arc::new(Mutex::new(None)),
        };
        let client_1 = JsonRpcClient::new(transport);
        let client_2 = client_1.clone();

        let fut1 = tokio::spawn(async move {
            let response = client_1.call_typed::<_, DummyResponse>(req).await.unwrap();
            assert_eq!(response.foo, "hello world!");
            assert_eq!(response.bar, 13);
        });

        let fut2 = tokio::spawn(async move {
            loop {
                let message = {
                    let mut lock = client_2.transport.req.lock().await;
                    lock.take()
                };
                if message.is_some() {
                    let msg = message.unwrap();
                    let req_val: Value = serde_json::from_str(&msg).unwrap();
                    let id = req_val.get("id").and_then(|id| id.as_str()).unwrap();
                    let res = DummyResponse {
                        foo: "hello world!".into(),
                        bar: 13,
                    }
                    .into_response(id.into());
                    let response_str = serde_json::to_string(&res).unwrap();
                    client_2.handle_response(response_str).await;
                    return;
                }
                time::sleep(Duration::from_millis(50)).await;
            }
        });

        let _ = join!(fut1, fut2);
    }

    #[tokio::test]
    async fn test_typed_call_w_error() {
        let req = DummyCall {
            foo: "hello world!".into(),
            bar: 13,
        };

        let transport = TestTransport {
            req: Arc::new(Mutex::new(None)),
        };
        let client_1 = JsonRpcClient::new(transport);
        let client_2 = client_1.clone();

        let fut1 = tokio::spawn(async move {
            let response = client_1.call_typed::<_, DummyResponse>(req).await;
            assert!(response.is_err());
            let err = response.unwrap_err();
            assert_eq!(err.code, -111111);
            assert_eq!(err.message, "error msg");
        });

        let fut2 = tokio::spawn(async move {
            loop {
                let message = {
                    let mut lock = client_2.transport.req.lock().await;
                    lock.take()
                };
                if message.is_some() {
                    let msg = message.unwrap();
                    let req_val: Value = serde_json::from_str(&msg).unwrap();
                    let id = req_val.get("id").and_then(|id| id.as_str()).unwrap();
                    let err = RpcError {
                        code: -111111,
                        message: "error msg".into(),
                        data: None,
                    }
                    .into_response(id.into());
                    let err_str = serde_json::to_string(&err).unwrap();
                    client_2.handle_response(err_str).await;
                    return;
                }
                time::sleep(Duration::from_millis(50)).await;
            }
        });

        let _ = join!(fut1, fut2);
    }
}
