pub mod anthropic;
pub mod azure_openai;
pub mod bedrock;
pub mod custom;
pub mod google;
pub mod openai;
pub mod traits;

pub use traits::AiProvider;

use futures::Stream;
use std::pin::Pin;
use traits::*;

/// Dyn-compatible version of `AiProvider` that boxes the future returned by
/// `chat_completion`. This is necessary because `AiProvider::chat_completion`
/// uses `impl Future` (RPITIT) which is not dyn-compatible.
///
/// All types implementing `AiProvider` automatically implement `DynAiProvider`.
pub trait DynAiProvider: Send + Sync {
    fn name(&self) -> &str;

    fn chat_completion_boxed(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>>
                + Send
                + '_,
        >,
    >;

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>>;
}

impl<T: AiProvider> DynAiProvider for T {
    fn name(&self) -> &str {
        AiProvider::name(self)
    }

    fn chat_completion_boxed(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ChatCompletionResponse, GatewayError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(AiProvider::chat_completion(self, request))
    }

    fn stream_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, GatewayError>> + Send>> {
        AiProvider::stream_chat_completion(self, request)
    }
}
