use std::fmt;

use async_trait::async_trait;
use jsonrpsee::core::{
    client::{BatchResponse, ClientT, Error},
    params::BatchRequestBuilder,
    traits::ToRpcParams,
    JsonRawValue,
};
use serde::de::DeserializeOwned;

use super::{ForNetwork, Network, TaggedClient};

#[derive(Debug)]
pub struct RawParams(Option<Box<JsonRawValue>>);

impl RawParams {
    fn new(params: impl ToRpcParams) -> Result<Self, serde_json::Error> {
        params.to_rpc_params().map(Self)
    }
}

impl ToRpcParams for RawParams {
    fn to_rpc_params(self) -> Result<Option<Box<JsonRawValue>>, serde_json::Error> {
        Ok(self.0)
    }
}

/// Object-safe version of [`ClientT`] + [`Clone`].
///
/// The implementation is fairly straightforward: [`RawParams`] is used as a catch-all params type,
/// and `serde_json::Value` is used as a catch-all response type.
#[doc(hidden)]
// ^ The internals of this trait are considered implementation details; it's only exposed via `DynClient` type alias
#[async_trait]
pub trait ObjectSafeClient: 'static + Send + Sync + fmt::Debug + ForNetwork {
    fn clone_boxed(&self) -> Box<dyn ObjectSafeClient<Net = Self::Net>>;

    fn for_component(
        self: Box<Self>,
        component_name: &'static str,
    ) -> Box<dyn ObjectSafeClient<Net = Self::Net>>;

    fn component(&self) -> &'static str;

    async fn notification(&self, method: &str, params: RawParams) -> Result<(), Error>;

    async fn request(&self, method: &str, params: RawParams) -> Result<serde_json::Value, Error>;

    async fn batch_request<'a>(
        &self,
        batch: BatchRequestBuilder<'a>,
    ) -> Result<BatchResponse<'a, serde_json::Value>, Error>;
}

#[async_trait]
impl<C> ObjectSafeClient for C
where
    C: 'static + Send + Sync + Clone + fmt::Debug + ClientT + TaggedClient,
{
    fn clone_boxed(&self) -> Box<dyn ObjectSafeClient<Net = <C as ForNetwork>::Net>> {
        Box::new(<C as Clone>::clone(self))
    }

    fn for_component(
        self: Box<Self>,
        component_name: &'static str,
    ) -> Box<dyn ObjectSafeClient<Net = <C as ForNetwork>::Net>> {
        Box::new(TaggedClient::for_component(*self, component_name))
    }

    fn component(&self) -> &'static str {
        TaggedClient::component(self)
    }

    async fn notification(&self, method: &str, params: RawParams) -> Result<(), Error> {
        <C as ClientT>::notification(self, method, params).await
    }

    async fn request(&self, method: &str, params: RawParams) -> Result<serde_json::Value, Error> {
        <C as ClientT>::request(self, method, params).await
    }

    async fn batch_request<'a>(
        &self,
        batch: BatchRequestBuilder<'a>,
    ) -> Result<BatchResponse<'a, serde_json::Value>, Error> {
        <C as ClientT>::batch_request(self, batch).await
    }
}

/// Dynamically typed RPC client for a certain [`Network`].
pub type DynClient<Net> = dyn ObjectSafeClient<Net = Net>;

impl<Net: Network> Clone for Box<DynClient<Net>> {
    fn clone(&self) -> Self {
        self.as_ref().clone_boxed()
    }
}

#[async_trait]
impl<Net: Network> ClientT for &DynClient<Net> {
    async fn notification<Params>(&self, method: &str, params: Params) -> Result<(), Error>
    where
        Params: ToRpcParams + Send,
    {
        (**self).notification(method, RawParams::new(params)?).await
    }

    async fn request<R, Params>(&self, method: &str, params: Params) -> Result<R, Error>
    where
        R: DeserializeOwned,
        Params: ToRpcParams + Send,
    {
        let raw_response = (**self).request(method, RawParams::new(params)?).await?;
        serde_json::from_value(raw_response).map_err(Error::ParseError)
    }

    async fn batch_request<'a, R>(
        &self,
        batch: BatchRequestBuilder<'a>,
    ) -> Result<BatchResponse<'a, R>, Error>
    where
        R: DeserializeOwned + fmt::Debug + 'a,
    {
        let raw_responses = (**self).batch_request(batch).await?;
        let mut successful_calls = 0;
        let mut failed_calls = 0;
        let mut responses = Vec::with_capacity(raw_responses.len());
        for raw_response in raw_responses {
            responses.push(match raw_response {
                Ok(json) => {
                    successful_calls += 1;
                    Ok(serde_json::from_value::<R>(json)?)
                }
                Err(err) => {
                    failed_calls += 1;
                    Err(err)
                }
            })
        }
        Ok(BatchResponse::new(
            successful_calls,
            responses,
            failed_calls,
        ))
    }
}

// Delegates to the above `&DynClient<Net>` implementation.
#[async_trait]
impl<Net: Network> ClientT for Box<DynClient<Net>> {
    async fn notification<Params>(&self, method: &str, params: Params) -> Result<(), Error>
    where
        Params: ToRpcParams + Send,
    {
        ClientT::notification(&self.as_ref(), method, params).await
    }

    async fn request<R, Params>(&self, method: &str, params: Params) -> Result<R, Error>
    where
        R: DeserializeOwned,
        Params: ToRpcParams + Send,
    {
        ClientT::request(&self.as_ref(), method, params).await
    }

    async fn batch_request<'a, R>(
        &self,
        batch: BatchRequestBuilder<'a>,
    ) -> Result<BatchResponse<'a, R>, Error>
    where
        R: DeserializeOwned + fmt::Debug + 'a,
    {
        ClientT::batch_request(&self.as_ref(), batch).await
    }
}

impl<Net: Network> TaggedClient for Box<DynClient<Net>> {
    fn for_component(self, component_name: &'static str) -> Self {
        ObjectSafeClient::for_component(self, component_name)
    }

    fn component(&self) -> &'static str {
        (**self).component()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::{MockClient, L2},
        namespaces::EthNamespaceClient,
    };

    #[tokio::test]
    async fn boxing_mock_client() {
        let client = MockClient::new(|method, params| {
            assert_eq!(method, "eth_blockNumber");
            assert_eq!(params, serde_json::Value::Null);
            Ok(serde_json::json!("0x42"))
        });
        let client = Box::new(client) as Box<DynClient<L2>>;

        let block_number = client.get_block_number().await.unwrap();
        assert_eq!(block_number, 0x42.into());
        let block_number = client.as_ref().get_block_number().await.unwrap();
        assert_eq!(block_number, 0x42.into());
    }

    #[tokio::test]
    async fn client_can_be_cloned() {
        let client = MockClient::new(|method, params| {
            assert_eq!(method, "eth_blockNumber");
            assert_eq!(params, serde_json::Value::Null);
            Ok(serde_json::json!("0x42"))
        });
        let client = Box::new(client) as Box<DynClient<L2>>;

        let cloned_client = client.clone();
        let block_number = cloned_client.get_block_number().await.unwrap();
        assert_eq!(block_number, 0x42.into());

        let client_with_label = client.for_component("test");
        assert_eq!(TaggedClient::component(&client_with_label), "test");
        let block_number = client_with_label.get_block_number().await.unwrap();
        assert_eq!(block_number, 0x42.into());
    }
}
