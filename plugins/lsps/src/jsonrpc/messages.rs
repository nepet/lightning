use serde::{de::DeserializeOwned, Deserialize, Serialize, Serializer};
use serde_json::{json, Value};

/// Trait to convert a struct into a JSON-RPC RequestObject.
pub trait JsonRpcRequest<'a>: Serialize {
    fn method_name() -> &'static str;
    fn into_request(self, id: impl Into<Option<Box<str>>>) -> RequestObject<'a, Self>
    where
        Self: Sized,
    {
        RequestObject {
            jsonrpc: "2.0",
            method: Self::method_name(),
            params: self,
            id: id.into(),
        }
    }
}

/// Trait for converting JSON-RPC responses into typed results.
pub trait JsonRpcResponse<T>
where
    T: DeserializeOwned,
{
    fn into_response(self, id: String) -> ResponseObject<Self>
    where
        Self: Sized + DeserializeOwned,
    {
        ResponseObject {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: Some(self),
            error: None,
        }
    }

    fn from_response(resp: ResponseObject<T>) -> Result<T, RpcError> {
        match (resp.result, resp.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(error),
            _ => Err(RpcError {
                code: -32603,
                message: "Invalid response format".into(),
                data: None,
            }),
        }
    }
}

/// This is a blanket trait implementation that automatically implements
/// `JsonRpcResponse` for all `Deserialize`-able structs.
impl<T> JsonRpcResponse<T> for T where T: DeserializeOwned {}

/// JSON-RPC 2.0 Request Structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct RequestObject<'a, T>
where
    T: Serialize,
{
    pub(crate) jsonrpc: &'static str,
    pub(crate) method: &'a str,
    #[serde(default)]
    #[serde(serialize_with = "force_empty_object")]
    pub(crate) params: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<Box<str>>,
}

impl<'a, T> RequestObject<'a, T>
where
    T: Serialize,
{
    /// Returns the inner data object.
    pub fn into_inner(self) -> T {
        self.params
    }
}

/// Custom serializer to ensure empty structs serialize as `{}` instead of
/// `null`.
fn force_empty_object<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize,
{
    match serde_json::to_value(value) {
        Ok(Value::Null) => serializer.serialize_some(&json!({})),
        Ok(other) => other.serialize(serializer),
        Err(e) => Err(serde::ser::Error::custom(e)),
    }
}

/// JSON-RPC 2.0 Response Structure.
#[derive(Debug, Serialize, Deserialize)]
#[serde(bound = "T: Serialize + DeserializeOwned")]
pub struct ResponseObject<T>
where
    T: DeserializeOwned,
{
    jsonrpc: String,
    id: Box<str>,
    //#[serde(default)]
    result: Option<T>,
    //#[serde(default)]
    error: Option<RpcError>,
}

impl<T> ResponseObject<T>
where
    T: DeserializeOwned + Serialize,
{
    /// Returns the inner data object.
    pub fn into_inner(self) -> Result<T, RpcError> {
        T::from_response(self)
    }
}

pub type RpcErrorResponse = ResponseObject<serde_json::Value>;

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl core::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let data = serde_json::to_string(&self.data).unwrap_or_default();
        write!(
            f,
            "code: {:?}, message: {}, data: {}",
            self.code,
            self.message.to_string(),
            data
        )
    }
}

impl RpcError {
    pub fn internal_error<D: core::fmt::Display>(message: D, data: Option<Value>) -> Self {
        Self {
            code: -32603,
            message: message.to_string(),
            data,
        }
    }

    pub fn into_response(self, id: String) -> RpcErrorResponse {
        ResponseObject {
            jsonrpc: "2.0".into(),
            id: id.into(),
            result: None,
            error: Some(self),
        }
    }
}

#[cfg(test)]
mod test_json_rpc {
    use super::*;

    #[test]
    fn test_empty_params_serialization() {
        // Empty params should serialize to `"params":{}` instead of
        // `"params":null`.
        #[derive(Debug, Serialize, Deserialize)]
        pub struct SayHelloRequest;
        impl<'a> JsonRpcRequest<'a> for SayHelloRequest {
            fn method_name() -> &'static str {
                "say_hello"
            }
        }
        let rpc_request = SayHelloRequest.into_request(Some("unique-id-123".into()));
        assert!(serde_json::to_string(&rpc_request)
            .expect("could not convert to json")
            .contains("\"params\":{}"));
    }

    #[test]
    fn test_request_serialization_and_deserialization() {
        // Ensure that we correctly serialize to a valid JSON-RPC 2.0 request.
        #[derive(Default, Debug, Serialize, Deserialize)]
        pub struct SayNameRequest {
            name: &'static str,
            age: i32,
        }
        impl<'a> JsonRpcRequest<'a> for SayNameRequest {
            fn method_name() -> &'static str {
                "say_name"
            }
        }
        let rpc_request = SayNameRequest {
            name: "Satoshi",
            age: 99,
        }
        .into_request(Some("unique-id-123".into()));
        let expected_result = r#"{"jsonrpc":"2.0","method":"say_name","params":{"age":99,"name":"Satoshi"},"id":"unique-id-123"}"#;
        assert_eq!(
            serde_json::to_string(&rpc_request).expect("could not convert to json"),
            expected_result
        );

        // Now check that we as well can deserialize this into it's correct
        // type.
        let request: RequestObject<serde_json::Value> =
            serde_json::from_str(&expected_result).unwrap();
        assert_eq!(request.method, "say_name");
        assert_eq!(request.jsonrpc, "2.0");

        let request: RequestObject<SayNameRequest> =
            serde_json::from_str(&expected_result).unwrap();
        let inner = request.into_inner();
        assert_eq!(inner.name, rpc_request.params.name);
    }

    #[test]
    fn test_response_deserialization() {
        // Check that we can convert a JSON-RPC response into a typed result.
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        pub struct SayNameResponse {
            name: String,
            age: i32,
            message: String,
        }

        let json_response = r#"
        {
            "jsonrpc": "2.0",
            "result": {
                "age": 99,
                "message": "Hello Satoshi!",
                "name": "Satoshi"
            },
            "id": "unique-id-123"
        }"#;

        let response_object: ResponseObject<SayNameResponse> =
            serde_json::from_str(json_response).unwrap();

        let response: SayNameResponse = response_object.into_inner().unwrap();
        let expected_response = SayNameResponse {
            name: "Satoshi".into(),
            age: 99,
            message: "Hello Satoshi!".into(),
        };

        assert_eq!(response, expected_response);
    }

    #[test]
    fn test_empty_result() {
        // Check that we correctly deserialize an empty result.
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        pub struct DummyResponse {}

        let json_response = r#"
        {
            "jsonrpc": "2.0",
            "result": {},
            "id": "unique-id-123"
        }"#;

        let response_object: ResponseObject<DummyResponse> =
            serde_json::from_str(json_response).unwrap();

        let response: DummyResponse = response_object.into_inner().unwrap();
        let expected_response = DummyResponse {};

        assert_eq!(response, expected_response);
    }
    #[test]
    fn test_error_deserialization() {
        // Check that we deserialize an error if we got one.
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        pub struct DummyResponse {}

        let json_response = r#"
        {
            "jsonrpc": "2.0",
            "id": "unique-id-123",
            "error": {
                "code": -32099,
                "message": "something bad happened",
                "data": {
                    "f1": "v1",
                    "f2": 2
                }
            }
        }"#;

        let response_object: ResponseObject<DummyResponse> =
            serde_json::from_str(json_response).unwrap();

        let response = response_object.into_inner();
        let err = response.unwrap_err();
        assert_eq!(err.code, -32099);
        assert_eq!(err.message, "something bad happened");
        assert_eq!(
            err.data,
            serde_json::from_str("{\"f1\":\"v1\",\"f2\":2}").unwrap()
        );
    }
}
