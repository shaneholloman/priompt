use anyhow::Context;
use async_channel::{bounded, Receiver, Sender};
use napi::bindgen_prelude::create_custom_tokio_runtime;
use napi::bindgen_prelude::Error;
use napi_derive::napi;
use once_cell::sync::Lazy;
use tiktoken::EncodingFactoryError;
use tokio::runtime::Builder;

use std::collections::HashMap;
use std::sync::Arc;

// we use the actor pattern to have good cache locality
// this means that no tokenization requests will ever run in parallel, but i think that's almost certainly fine
use napi::tokio::sync::oneshot;

static TOKENIZER: Lazy<Result<Tokenizer, Error>> =
  Lazy::new(|| Tokenizer::new().map_err(|e| Error::from_reason(e.to_string())));

static ENCODINGS: Lazy<Result<Arc<Encodings>, EncodingFactoryError>> = Lazy::new(|| {
  Ok(Arc::new(Encodings {
    cl100k_encoding: tiktoken::EncodingFactory::cl100k_im()?,
    llama3_encoding: tiktoken::EncodingFactory::llama3()?,
    o200k_encoding: tiktoken::EncodingFactory::o200k_im()?,
    codestral_encoding: tiktoken::EncodingFactory::codestral()?,
  }))
});

#[napi]
pub enum SupportedEncoding {
  Cl100k = 0,
  Llama3 = 1,
  O200k = 2,
  Codestral = 3,
}

struct TokenizerActor {
  receiver: Receiver<TokenizerMessage>,
  encodings: Arc<Encodings>,
}

struct Encodings {
  cl100k_encoding: tiktoken::Encoding,
  llama3_encoding: tiktoken::Encoding,
  o200k_encoding: tiktoken::Encoding,
  codestral_encoding: tiktoken::Encoding,
}

enum TokenizerMessage {
  ExactNumTokens {
    respond_to: oneshot::Sender<anyhow::Result<i32>>,
    text: String,
    encoding: SupportedEncoding,
    special_token_handling: tiktoken::SpecialTokenHandling,
  },
  EncodeTokens {
    respond_to: oneshot::Sender<anyhow::Result<Vec<u32>>>,
    text: String,
    encoding: SupportedEncoding,
    special_token_handling: tiktoken::SpecialTokenHandling,
  },
  // always encodes all special tokens!
  EncodeSingleToken {
    respond_to: oneshot::Sender<anyhow::Result<u32>>,
    bytes: Vec<u8>,
    encoding: SupportedEncoding,
  },
  DecodeTokens {
    respond_to: oneshot::Sender<anyhow::Result<String>>,
    tokens: Vec<u32>,
    encoding: SupportedEncoding,
  },
  DecodeTokenBytes {
    respond_to: oneshot::Sender<anyhow::Result<Vec<u8>>>,
    token: u32,
    encoding: SupportedEncoding,
  },
  ApproximateNumTokens {
    respond_to: oneshot::Sender<anyhow::Result<i32>>,
    text: String,
    encoding: SupportedEncoding,
    replace_spaces_with_lower_one_eighth_block: bool,
  },
}

impl TokenizerActor {
  fn new(receiver: Receiver<TokenizerMessage>, encodings: Arc<Encodings>) -> Self {
    TokenizerActor { receiver, encodings }
  }

  fn get_encoding(&self, encoding: SupportedEncoding) -> &tiktoken::Encoding {
    match encoding {
      SupportedEncoding::Cl100k => &self.encodings.cl100k_encoding,
      SupportedEncoding::Llama3 => &self.encodings.llama3_encoding,
      SupportedEncoding::O200k => &self.encodings.o200k_encoding,
      SupportedEncoding::Codestral => &self.encodings.codestral_encoding,
    }
  }

  fn handle_message(&self, msg: TokenizerMessage) {
    match msg {
      TokenizerMessage::ExactNumTokens { respond_to, text, encoding, special_token_handling } => {
        let tokens = self
          .get_encoding(encoding)
          .encode(&text, &special_token_handling)
          .context("Error encoding string");

        let num_tokens = match tokens {
          Ok(t) => Ok(t.len() as i32),
          Err(e) => Err(e),
        };

        // The `let _ =` ignores any errors when sending.
        let _ = respond_to.send(num_tokens);
      }
      TokenizerMessage::EncodeTokens { respond_to, text, encoding, special_token_handling } => {
        let tokens = self
          .get_encoding(encoding)
          .encode(&text, &special_token_handling)
          .context("Error encoding string");

        let tokens = match tokens {
          Ok(t) => Ok(t.into_iter().map(|t| t as u32).collect()),
          Err(e) => Err(e),
        };

        // The `let _ =` ignores any errors when sending.
        let _ = respond_to.send(tokens);
      }
      TokenizerMessage::EncodeSingleToken { respond_to, bytes, encoding } => {
        let token = self.get_encoding(encoding).encode_single_token_bytes(&bytes);

        let token = match token {
          Ok(t) => Ok(t as u32),
          Err(_) => Err(anyhow::anyhow!("Token not recognized")),
        };

        // The `let _ =` ignores any errors when sending.
        let _ = respond_to.send(token);
      }
      TokenizerMessage::DecodeTokenBytes { respond_to, token, encoding } => {
        let bytes = self.get_encoding(encoding).decode_single_token_bytes(token as usize);
        let bytes = match bytes {
          Ok(b) => Ok(b),
          Err(e) => Err(anyhow::anyhow!(e)),
        };
        let _ = respond_to.send(bytes);
      }
      TokenizerMessage::DecodeTokens { respond_to, tokens, encoding } => {
        let text = self
          .get_encoding(encoding)
          .decode(&tokens.into_iter().map(|t| t as usize).collect::<Vec<_>>());

        // The `let _ =` ignores any errors when sending.
        let _ = respond_to.send(Ok(text));
      }
      TokenizerMessage::ApproximateNumTokens {
        respond_to,
        text,
        encoding,
        replace_spaces_with_lower_one_eighth_block,
      } => {
        let tokens = self.get_encoding(encoding).estimate_num_tokens_no_special_tokens_fast(
          &text,
          replace_spaces_with_lower_one_eighth_block,
        );

        // The `let _ =` ignores any errors when sending.
        let _ = respond_to.send(Ok(tokens as i32));
      }
    }
  }
}

fn run_tokenizer_actor(actor: TokenizerActor) {
  while let Ok(msg) = actor.receiver.recv_blocking() {
    actor.handle_message(msg);
  }
}

#[napi]
#[derive(Clone)]
pub struct Tokenizer {
  sender: Sender<TokenizerMessage>,
}

#[napi]
pub enum SpecialTokenAction {
  /// The special token is forbidden. If it is included in the string, an error will be returned.
  Forbidden = 0,
  /// The special token is tokenized as normal text.
  NormalText = 1,
  /// The special token is treated as the special token it is. If this is applied to a specific text and the text is NOT a special token then an error will be returned. If it is the default action no error will be returned, don't worry.
  Special = 2,
}

impl SpecialTokenAction {
  pub fn to_tiktoken(&self) -> tiktoken::SpecialTokenAction {
    match self {
      SpecialTokenAction::Forbidden => tiktoken::SpecialTokenAction::Forbidden,
      SpecialTokenAction::NormalText => tiktoken::SpecialTokenAction::NormalText,
      SpecialTokenAction::Special => tiktoken::SpecialTokenAction::Special,
    }
  }
}

#[napi]
impl Tokenizer {
  pub fn new() -> Result<Self, tiktoken::EncodingFactoryError> {
    let (sender, receiver) = bounded(256);
    for i in 0..4 {
      let actor = TokenizerActor::new(receiver.clone(), ENCODINGS.clone().unwrap());
      std::thread::Builder::new()
        .name(format!("tokenizer-actor-{}", i))
        .spawn(move || run_tokenizer_actor(actor))
        .unwrap();
    }

    Ok(Self { sender })
  }

  #[napi]
  pub async fn exact_num_tokens_no_special_tokens(
    &self,
    text: String,
    encoding: SupportedEncoding,
  ) -> Result<i32, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::ExactNumTokens {
      respond_to: send,
      text,
      encoding,
      special_token_handling: tiktoken::SpecialTokenHandling {
        // no special tokens!! everything is normal text
        // this is how tokenization is handled in the chat model api
        default: tiktoken::SpecialTokenAction::NormalText,
        ..Default::default()
      },
    };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }

  #[napi]
  pub async fn exact_num_tokens(
    &self,
    text: String,
    encoding: SupportedEncoding,
    special_token_default_action: SpecialTokenAction,
    special_token_overrides: HashMap<String, SpecialTokenAction>,
  ) -> Result<i32, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::ExactNumTokens {
      respond_to: send,
      text,
      encoding,
      special_token_handling: tiktoken::SpecialTokenHandling {
        // no special tokens!! everything is normal text
        // this is how tokenization is handled in the chat model api
        default: special_token_default_action.to_tiktoken(),
        overrides: special_token_overrides.into_iter().map(|(k, v)| (k, v.to_tiktoken())).collect(),
      },
    };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }

  #[napi]
  pub async fn encode_cl100k_no_special_tokens(&self, text: String) -> Result<Vec<u32>, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::EncodeTokens {
      respond_to: send,
      text,
      encoding: SupportedEncoding::Cl100k,
      special_token_handling: tiktoken::SpecialTokenHandling {
        // no special tokens!! everything is normal text
        // this is how tokenization is handled in the chat model api
        default: tiktoken::SpecialTokenAction::NormalText,
        ..Default::default()
      },
    };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }

  #[napi]
  pub async fn approx_num_tokens(
    &self,
    text: String,
    encoding: SupportedEncoding,
    replace_spaces_with_lower_one_eighth_block: bool,
  ) -> Result<i32, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::ApproximateNumTokens {
      respond_to: send,
      text,
      encoding,
      replace_spaces_with_lower_one_eighth_block,
    };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }

  #[napi]
  pub async fn encode(
    &self,
    text: String,
    encoding: SupportedEncoding,
    special_token_default_action: SpecialTokenAction,
    special_token_overrides: HashMap<String, SpecialTokenAction>,
  ) -> Result<Vec<u32>, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::EncodeTokens {
      respond_to: send,
      text,
      encoding,
      special_token_handling: tiktoken::SpecialTokenHandling {
        // no special tokens!! everything is normal text
        // this is how tokenization is handled in the chat model api
        default: special_token_default_action.to_tiktoken(),
        overrides: special_token_overrides.into_iter().map(|(k, v)| (k, v.to_tiktoken())).collect(),
      },
    };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }

  #[napi]
  pub async fn encode_single_token(
    &self,
    bytes: napi::bindgen_prelude::Uint8Array,
    encoding: SupportedEncoding,
  ) -> Result<u32, Error> {
    let (send, recv) = oneshot::channel();
    let msg =
      TokenizerMessage::EncodeSingleToken { respond_to: send, bytes: bytes.to_vec(), encoding };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }
  #[napi]
  pub async fn decode_byte(
    &self,
    token: u32,
    encoding: SupportedEncoding,
  ) -> Result<napi::bindgen_prelude::Uint8Array, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::DecodeTokenBytes { respond_to: send, token, encoding };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result
        .map_err(|e| napi::Error::from_reason(e.to_string()))
        .map(|v| napi::bindgen_prelude::Uint8Array::new(v.into())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }

  #[napi]
  pub async fn decode(
    &self,
    encoded_tokens: Vec<u32>,
    encoding: SupportedEncoding,
  ) -> Result<String, Error> {
    let (send, recv) = oneshot::channel();
    let msg = TokenizerMessage::DecodeTokens { respond_to: send, tokens: encoded_tokens, encoding };

    // ignore errors since it can only mean the channel is closed, which will be caught in the recv below
    let _ = self.sender.send(msg).await;
    match recv.await {
      Ok(result) => result.map_err(|e| Error::from_reason(e.to_string())),
      Err(e) => Err(Error::from_reason(format!("Actor task has been killed: {}", e.to_string()))),
    }
  }
}

#[napi]
pub struct SyncTokenizer {
  encodings: Arc<Encodings>,
}

#[napi]
impl SyncTokenizer {
  #[napi(constructor)]
  pub fn new() -> Result<Self, napi::Error> {
    Ok(Self { encodings: ENCODINGS.clone().unwrap() })
  }

  #[napi]
  pub fn approx_num_tokens(&self, text: String, encoding: SupportedEncoding) -> Result<i32, Error> {
    Ok(self.get_encoding(encoding).estimate_num_tokens_no_special_tokens_fast(&text, false) as i32)
  }

  fn get_encoding(&self, encoding: SupportedEncoding) -> &tiktoken::Encoding {
    match encoding {
      SupportedEncoding::Cl100k => &self.encodings.cl100k_encoding,
      SupportedEncoding::Llama3 => &self.encodings.llama3_encoding,
      SupportedEncoding::O200k => &self.encodings.o200k_encoding,
      SupportedEncoding::Codestral => &self.encodings.codestral_encoding,
    }
  }
}

#[napi]
pub fn get_tokenizer() -> Result<Tokenizer, Error> {
  TOKENIZER.clone()
}

#[allow(clippy::expect_used)]
#[napi::module_init]
fn init() {
  let rt = Builder::new_multi_thread()
    .enable_all()
    .worker_threads(2)
    .thread_name("tokenizer-tokio")
    .build()
    .expect("Failed to build tokio runtime");
  create_custom_tokio_runtime(rt);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn test_num_tokens() {
    let tokenizer = get_tokenizer().unwrap();
    let num_tokens = tokenizer
      .exact_num_tokens_no_special_tokens("hello, world".to_string(), SupportedEncoding::Cl100k)
      .await
      .unwrap();
    assert_eq!(num_tokens, 3);
  }
}
