use super::model::{EvaluateOutputRequest, Model};
use super::token::{OutputToken, TokenBias, TokenId, EOD_TOKEN_ID};
use super::vocabulary::Vocabulary;
use core::slice;
use std::fmt::Display;
use std::time::SystemTime;
use thiserror::Error;

/// An inference session represents the state of the text generation. This holds
/// the full context window, as long as several additional parameters used
/// during sampling.
pub struct InferenceSession {
    // Must be kept alive for the model
    pub _session_ctx: ggml::Context,

    // Parameters for the session.
    pub params: InferenceSessionParameters,

    pub memory_k: ggml::Tensor,
    pub memory_v: ggml::Tensor,

    /// How many tokens have been fed into the model's working memory so far.
    pub n_past: usize,

    /// How much memory is required per token for the temporary context used
    /// during inference.
    pub mem_per_token: usize,

    /// All tokens generated by this inference session
    pub tokens: Vec<TokenId>,

    /// The logits that were last predicted by the network. Zeroed out otherwise.
    pub last_logits: Vec<f32>,
}
impl InferenceSession {
    pub fn repetition_penalty_tokens(&self) -> &[TokenId] {
        &self.tokens[self
            .tokens
            .len()
            .saturating_sub(self.params.repetition_penalty_last_n)..]
    }
}

#[derive(serde::Serialize, Clone, PartialEq)]
/// A serializable snapshot of the inference process. Can be saved to disk.
// Keep in sync with [InferenceSession] and [InferenceSnapshot]
pub struct InferenceSnapshotRef<'a> {
    /// How many tokens have been stored in the memory so far.
    pub npast: usize,
    // Parameters associated with the saved inference session.
    pub session_params: InferenceSessionParameters,
    /// All tokens generated by this inference session
    pub tokens: Vec<TokenId>,
    /// The vector of logits that was produced after the last inference
    pub logits: Vec<f32>,
    /// The contents of the 'key' memory tensor
    #[serde(with = "serde_bytes")]
    pub memory_k: &'a [u8],
    /// The contents of the 'value' memory tensor
    #[serde(with = "serde_bytes")]
    pub memory_v: &'a [u8],
}

/// A serializable snapshot of the inference process. Can be restored by calling
/// `Model::restore_from_snapshot`.
#[derive(serde::Deserialize, Clone, PartialEq)]
// Keep in sync with [InferenceSession] and [InferenceSnapshotRef]
pub struct InferenceSnapshot {
    /// How many tokens have been stored in the memory so far.
    pub npast: usize,
    // Parameters associated with the saved inference session.
    pub session_params: InferenceSessionParameters,
    /// All tokens generated by this inference session
    pub tokens: Vec<TokenId>,
    /// The vector of logitsTokenB that was produced after the last inference
    pub last_logits: Vec<f32>,
    /// The contents of the 'key' memory tensor
    #[serde(with = "serde_bytes")]
    pub memory_k: Vec<u8>,
    /// The contents of the 'value' memory tensor
    #[serde(with = "serde_bytes")]
    pub memory_v: Vec<u8>,
}

// Allowed types for the model memory K/V tensors.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ModelKVMemoryType {
    Float16,
    Float32,
}

impl From<ModelKVMemoryType> for i32 {
    fn from(value: ModelKVMemoryType) -> Self {
        match value {
            ModelKVMemoryType::Float16 => ggml::TYPE_F16,
            ModelKVMemoryType::Float32 => ggml::TYPE_F32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
// Parameters for an inference session.
pub struct InferenceSessionParameters {
    pub repetition_penalty_last_n: usize,
    pub memory_k_type: ModelKVMemoryType,
    pub memory_v_type: ModelKVMemoryType,
}

impl Default for InferenceSessionParameters {
    fn default() -> Self {
        Self {
            repetition_penalty_last_n: 512,
            memory_k_type: ModelKVMemoryType::Float32,
            memory_v_type: ModelKVMemoryType::Float32,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
/// The parameters that drive text generation.
pub struct InferenceParameters {
    pub n_threads: usize,
    pub n_batch: usize,
    pub top_k: usize,
    pub top_p: f32,
    pub repeat_penalty: f32,
    pub temp: f32,
    pub bias_tokens: TokenBias,
    pub play_back_previous_tokens: bool,
    pub increased_determinism: bool,
}

impl Default for InferenceParameters {
    fn default() -> Self {
        Self {
            n_threads: 8,
            n_batch: 8,
            top_k: 40,
            top_p: 0.95,
            repeat_penalty: 1.30,
            temp: 0.80,
            bias_tokens: TokenBias::default(),
            play_back_previous_tokens: false,
            increased_determinism: true,
        }
    }
}

pub struct InferenceStats {
    pub feed_prompt_duration: std::time::Duration,
    pub prompt_tokens: usize,
    pub predict_duration: std::time::Duration,
    pub predict_tokens: usize,
}

impl Default for InferenceStats {
    fn default() -> Self {
        Self {
            feed_prompt_duration: std::time::Duration::from_secs(0),
            prompt_tokens: 0,
            predict_duration: std::time::Duration::from_secs(0),
            predict_tokens: 0,
        }
    }
}

impl Display for InferenceStats {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "feed_prompt_duration: {}ms\nprompt_tokens: {}\npredict_duration: {}ms\npredict_tokens: {}\nper_token_duration: {:.3}ms",
            self.feed_prompt_duration.as_millis(),
            self.prompt_tokens,
            self.predict_duration.as_millis(),
            self.predict_tokens,
            (self.predict_duration.as_millis() as f64) / (self.predict_tokens as f64),
        )
    }
}

impl InferenceSession {
    pub fn feed_prompt<E: std::error::Error + 'static, M: Model>(
        &mut self,
        model: &M,
        vocab: &Vocabulary,
        params: &InferenceParameters,
        prompt: &str,
        callback: impl Fn(OutputToken) -> Result<(), E>,
    ) -> Result<(), InferenceError> {
        let beginning_of_sentence = self.n_past == 0;
        let prompt_tokens = model.tokenize(vocab, prompt, beginning_of_sentence)?;

        //if self.n_past + prompt_tokens.len() >= model.hparams.n_ctx as usize {
        //return Err(InferenceError::ContextFull);
        //}

        for batch in prompt_tokens.chunks(8) {
            model.evaluate(self, params, batch, &mut EvaluateOutputRequest::default());
            for &tk in batch {
                // NOTE: No string ever tokenizes to the end of sentence. So we
                // can just return the id here.
                if let Err(e) = callback(OutputToken::Token(&vocab.id_to_token[tk as usize])) {
                    return Err(InferenceError::UserCallback(Box::new(e)));
                }

                // Update the tokens for this session
                self.tokens.push(tk);
            }
        }

        Ok(())
    }

    pub fn infer_next_token<'v, M: Model>(
        &mut self,
        model: &M,
        vocab: &'v Vocabulary,
        params: &InferenceParameters,
        rng: &mut impl rand::Rng,
    ) -> Result<OutputToken<'v>, InferenceError> {
        //if self.n_past + 1 >= model.hparams.n_ctx as usize {
        //return Err(InferenceError::ContextFull);
        //}

        // First, sample the next token, using the stored last_logits;
        let next_token = model.sample_top_p_top_k(self, params, rng);

        // Update the tokens for this session
        self.tokens.push(next_token);

        // Then, evaluate the network again to compute the new last_logits
        model.evaluate(
            self,
            params,
            &[next_token],
            &mut EvaluateOutputRequest::default(),
        );

        // Return the next token
        Ok(if next_token as TokenId == EOD_TOKEN_ID {
            OutputToken::EndOfText
        } else {
            OutputToken::Token(&vocab.id_to_token[next_token as usize])
        })
    }

    // todo: see if we can reduce the arguments here somehow - consolidate model and vocab maybe?
    /// Helper function to run inference with this session and the given model and vocabulary.
    ///
    /// Note that this will "play back" all existing tokens in the session. If this is not desired
    /// behaviour, consider implementing your own inference loop to customize the behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn inference_with_prompt<E: std::error::Error + 'static, M: Model>(
        &mut self,
        model: &M,
        vocab: &Vocabulary,
        params: &InferenceParameters,
        prompt: &str,
        maximum_token_count: Option<usize>,
        rng: &mut impl rand::Rng,
        callback: impl Fn(OutputToken) -> Result<(), E>,
    ) -> Result<InferenceStats, InferenceError> {
        let maximum_token_count = maximum_token_count.unwrap_or(usize::MAX);
        if params.play_back_previous_tokens {
            // "Play back" the existing tokens, so that loading from an inference snapshot works
            // as expected.
            for token_id in &self.tokens {
                let token = OutputToken::from_id(vocab, *token_id);
                if let Err(e) = callback(token) {
                    return Err(InferenceError::UserCallback(Box::new(e)));
                }
            }
        }

        let mut stats = InferenceStats::default();

        let start_at = SystemTime::now();

        // Feed the initial prompt through the transformer, to update its
        // context window with new data.
        self.feed_prompt(model, vocab, params, prompt, |tk| callback(tk))?;
        stats.feed_prompt_duration = start_at.elapsed().unwrap();
        stats.prompt_tokens = self.n_past;

        // After the prompt is consumed, sample tokens by repeatedly calling
        // `infer_next_token`. We generate tokens until the model returns an
        // EndOfText token, or we run out of space in the context window,
        // or we reach the specified limit.
        let mut tokens_processed = 0;
        while tokens_processed < maximum_token_count {
            let token = self.infer_next_token(model, vocab, params, rng)?;

            if let Err(e) = callback(token) {
                return Err(InferenceError::UserCallback(Box::new(e)));
            }

            tokens_processed += 1;

            if let OutputToken::EndOfText = token {
                break;
            }
        }
        stats.predict_duration = start_at.elapsed().unwrap();
        stats.predict_tokens = self.n_past;

        Ok(stats)
    }

    /// Obtains a serializable snapshot of the current inference status. This
    /// can be used to cache the state of the model and store them into a file.
    ///
    /// # Safety
    ///
    /// This function provides raw access to the underlying memory owned by the
    /// ggml context. While the provided `InferenceSnapshotRef` object is alive,
    /// no other methods for this model object should be called.
    pub unsafe fn get_snapshot(&mut self) -> InferenceSnapshotRef<'_> {
        let memory_k = unsafe {
            slice::from_raw_parts(self.memory_k.data() as *mut u8, self.memory_k.nbytes())
        };
        let memory_v = unsafe {
            slice::from_raw_parts(self.memory_v.data() as *mut u8, self.memory_v.nbytes())
        };

        InferenceSnapshotRef {
            npast: self.n_past,
            session_params: self.params,
            tokens: self.tokens.clone(),
            logits: self.last_logits.clone(),
            memory_k,
            memory_v,
        }
    }
}

impl<'a> InferenceSnapshotRef<'a> {
    pub fn write(&self, writer: &mut impl std::io::Write) -> Result<(), SnapshotError> {
        Ok(bincode::serialize_into(writer, &self)?)
    }
}

impl InferenceSnapshot {
    pub fn read(reader: &mut impl std::io::Read) -> Result<Self, SnapshotError> {
        Ok(bincode::deserialize_from(reader)?)
    }
}

#[derive(Error, Debug)]
pub enum InferenceError {
    #[error("an invalid token was encountered during tokenization")]
    TokenizationFailed,
    #[error("the context window is full")]
    ContextFull,
    #[error("the user-specified callback returned an error")]
    UserCallback(Box<dyn std::error::Error>),
}

#[derive(Error, Debug)]
pub enum SnapshotError {
    #[error("I/O error while reading or writing snapshot")]
    IO(#[from] std::io::Error),
    #[error("error during snapshot serialization")]
    Serialization(#[from] bincode::Error),
    #[error("could not read snapshot due to size mismatch (self={self_size}, input={input_size})")]
    MemorySizeMismatch { self_size: usize, input_size: usize },
}
