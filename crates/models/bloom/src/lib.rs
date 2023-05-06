//! An implementation of [BLOOM](https://huggingface.co/docs/transformers/model_doc/bloom)
//! for the `llm` ecosystem.
//!
//! This implementation of BLOOM may not be fully correct. More work may be required.
#![deny(missing_docs)]

use std::path::Path;

use ggml::Tensor;

use llm_base::{
    ggml, model::common, util, EvaluateOutputRequest, FileType, InferenceParameters,
    InferenceSession, InferenceSessionParameters, InferenceWithPromptParameters, KnownModel,
    LoadError, LoadProgress, Mmap, ModelParameters, TokenId, Vocabulary,
};

/// The BLOOM model. Ref: [Introducing BLOOM](https://bigscience.huggingface.co/blog/bloom)
///
/// # Safety
/// This implements [Send] and [Sync] as it is immutable after construction.
pub struct Bloom {
    hyperparameters: Hyperparameters,
    n_context_tokens: usize,

    vocabulary: Vocabulary,
    tok_embeddings: Tensor,
    norm: Tensor,
    norm_b: Tensor,
    output_norm: Tensor,
    output_norm_b: Tensor,
    output: Tensor,
    layers: Vec<Layer>,

    inference_params: InferenceParameters,
    inference_prompt_params: InferenceWithPromptParameters,

    // Must be kept alive for the model
    _context: ggml::Context,
    _mmap: Option<Mmap>,
}
unsafe impl Send for Bloom {}
unsafe impl Sync for Bloom {}
impl Bloom {
    /// Load the model from `path` with `n_context_tokens` context tokens.
    ///
    /// The status of the loading process will be reported through `load_progress_callback`.
    pub fn load(
        path: &Path,
        params: ModelParameters,
        load_progress_callback: impl FnMut(LoadProgress),
    ) -> Result<Bloom, LoadError> {
        llm_base::load(path, params, load_progress_callback)
    }
}
impl KnownModel for Bloom {
    type Hyperparameters = Hyperparameters;

    fn new<E: std::error::Error>(
        hyperparameters: Self::Hyperparameters,
        params: ModelParameters,
        vocabulary: Vocabulary,
        tensor_loader: impl llm_base::TensorLoader<E>,
    ) -> Result<Self, E> {
        let mut tl = tensor_loader;

        let tok_embeddings = tl.load("tok_embeddings.weight")?;

        let norm = tl.load("norm.weight")?;
        let norm_b = tl.load("norm.bias")?;

        let output_norm = tl.load("output_norm.weight")?;
        let output_norm_b = tl.load("output_norm.bias")?;

        let output = tl.load("output.weight")?;

        let mut layers = Vec::new();
        for i in 0..hyperparameters.n_layer {
            let layer = Layer {
                attention_norm: tl.load(&format!("layers.{i}.attention_norm.weight"))?,
                attention_norm_b: tl.load(&format!("layers.{i}.attention_norm.bias"))?,

                query_key_value: tl
                    .load(&format!("layers.{i}.attention.query_key_value.weight"))?,
                query_key_value_b: tl
                    .load(&format!("layers.{i}.attention.query_key_value.bias"))?,

                wo: tl.load(&format!("layers.{i}.attention.wo.weight"))?,
                wo_b: tl.load(&format!("layers.{i}.attention.wo.bias"))?,

                ffn_norm: tl.load(&format!("layers.{i}.ffn_norm.weight"))?,
                ffn_norm_b: tl.load(&format!("layers.{i}.ffn_norm.bias"))?,

                w1: tl.load(&format!("layers.{i}.feed_forward.w1.weight"))?,
                w1_b: tl.load(&format!("layers.{i}.feed_forward.w1.bias"))?,
                w2: tl.load(&format!("layers.{i}.feed_forward.w2.weight"))?,
                w2_b: tl.load(&format!("layers.{i}.feed_forward.w2.bias"))?,
            };

            layers.push(layer);
        }

        let (_context, _, _mmap) = tl.finish();

        let ModelParameters {
            n_context_tokens,
            inference_params,
            inference_prompt_params,
            ..
        } = params;

        Ok(Bloom {
            hyperparameters,
            n_context_tokens,
            vocabulary,
            tok_embeddings,
            norm,
            norm_b,
            output_norm,
            output_norm_b,
            output,
            layers,
            inference_params,
            inference_prompt_params,
            _context,
            _mmap,
        })
    }

    fn start_session(&self, params: InferenceSessionParameters) -> InferenceSession {
        InferenceSession::new(
            params,
            self.n_context_tokens,
            self.hyperparameters.n_layer,
            self.hyperparameters.n_embd,
            self.hyperparameters.n_vocab,
        )
    }

    fn evaluate(
        &self,
        session: &mut InferenceSession,
        params: &InferenceParameters,
        input_tokens: &[TokenId],
        output_request: &mut EvaluateOutputRequest,
    ) {
        let n = input_tokens.len();
        let n_past = session.n_past;
        let n_threads = params.n_threads;

        let Hyperparameters {
            n_vocab,
            n_embd,
            n_mult: _,
            n_head,
            n_layer,
            file_type: _,
        } = self.hyperparameters;
        let n_ctx = self.n_context_tokens;

<<<<<<< HEAD
        // For the first run, we need to guess a maximum buffer size so we can measure
        // the actual memory consumption of the temporary ggml context.
        let mut buf_size = 1024 * 1024 * 1024;
        if session.mem_per_token > 0 && session.mem_per_token * n > buf_size {
            // add 10% to account for ggml object overhead
            buf_size = (1.1f64 * session.mem_per_token as f64 * n as f64) as usize;
        };
        let ctx0 = ggml::Context::init(buf_size, true);

        let mut gf = ggml::ComputationGraph::new(n_threads);

        let mut embd = ctx0.new_tensor_1d(ggml::Type::I32, n);
        unsafe { embd.write_data(bytemuck::cast_slice(input_tokens)) };
=======
        let (ctx0, embd) = common::prepare_for_evaluate(n_layer, session, input_tokens);
>>>>>>> main

        let mut input_layer = ctx0.op_get_rows(&self.tok_embeddings, &embd);

        // word embeddings norm,
        {
            input_layer = ctx0.op_norm(&input_layer);
            input_layer = ctx0.op_mul(&ctx0.op_repeat(&self.norm, &input_layer), &input_layer);
            input_layer = ctx0.op_add(&ctx0.op_repeat(&self.norm_b, &input_layer), &input_layer);
        }

        let mut gf = ggml::ComputationGraph::new(n_threads);

        for il in 0..n_layer {
            let input_self_attention = input_layer.share();
            let mut current: Tensor;

            // norm
            {
                current = ctx0.op_norm(&input_layer);

                // cur = attention_norm * cur
                current = ctx0.op_mul(
                    &ctx0.op_repeat(&self.layers[il].attention_norm, &current),
                    &current,
                );
                current = ctx0.op_add(
                    &ctx0.op_repeat(&self.layers[il].attention_norm_b, &current),
                    &current,
                );
            }

            //attention
            {
                current = ctx0.op_mul_mat(&self.layers[il].query_key_value, &current);
                current = ctx0.op_add(
                    &ctx0.op_repeat(&self.layers[il].query_key_value_b, &current),
                    &current,
                );
            }

            // self-attention
            {
                let nb = current.get_nb()[1];

                let q_current = ctx0.op_view_2d(
                    &current,
                    (n_embd, n),
                    nb,
                    0,
                );
                let k_current = ctx0.op_view_2d(
                    &current,
                    (n_embd, n),
                    nb,
                    std::mem::size_of::<f32>() * n_embd,
                );
                let v_current = ctx0.op_view_2d(
                    &current,
                    (n_embd, n),
                    nb,
                    2 * std::mem::size_of::<f32>() * n_embd,
                );

                // store key and value to memory
                if n >= 1 {
                    let k = ctx0.op_view_1d(
                        &session.memory_k,
                        n * n_embd,
                        (session.memory_k.element_size() * n_embd) * (il * n_ctx + n_past),
                    );

                    let v = ctx0.op_view_1d(
                        &session.memory_v,
                        n * n_embd,
                        (session.memory_v.element_size() * n_embd) * (il * n_ctx + n_past),
                    );

                    gf.build_forward_expand(&ctx0.op_cpy(&k_current, &k));
                    gf.build_forward_expand(&ctx0.op_cpy(&v_current, &v));
                }

                // Q = Qcur.contiguous().view(n_embd/n_head, n_head, N).permute(0, 2, 1, 3)
                let big_q = ctx0.op_permute(
                    &ctx0.op_cpy(
                        &q_current,
                        &ctx0.new_tensor_3d(ggml::Type::F32, n_embd / n_head, n_head, n),
                    ),
                    0,
                    2,
                    1,
                    3,
                );

                // K = Kmem.view(n_embd/n_head, n_head, n_past + N).permute(0, 2, 1, 3)
                let big_k = ctx0.op_permute(
                    &ctx0.op_reshape_3d(
                        &ctx0.op_view_1d(
                            &session.memory_k,
                            (n_past + n) * n_embd,
                            il * n_ctx * session.memory_k.element_size() * n_embd,
                        ),
                        n_embd / n_head,
                        n_head,
                        n_past + n,
                    ),
                    0,
                    2,
                    1,
                    3,
                );

                // K * Q
                let k_q = ctx0.op_mul_mat(&big_k, &big_q);

                // KQ_scaled = KQ / sqrt(n_embd/n_head)
                let k_q_scaled = ctx0.op_scale(
                    &k_q,
                    &ctx0.new_f32(1.0 / f32::sqrt(n_embd as f32 / n_head as f32)),
                );

                //alibi
                // KQ_scaled_alibi = KQ_scaled + alibi_bias
                let k_q_scaled_alibi = ctx0.op_alibi(&k_q_scaled, n_past, n_head);

                // KQ_masked = mask_past(KQ_scaled)
                let k_q_masked = ctx0.op_diag_mask_inf(&k_q_scaled_alibi, n_past);

                // KQ = soft_max(KQ_masked)
                let k_q_soft_max = ctx0.op_soft_max(&k_q_masked);

                let memv_elsize = session.memory_v.element_size();

                // let v_trans = ctx0.op_permute(
                //     &ctx0.op_reshape_3d(
                //         &ctx0.op_view_1d(
                //             &session.memory_v,
                //             (n_past + n) * n_embd,
                //             il * n_ctx * memv_elsize * n_embd,
                //         ),
                //         n_embd / n_head,
                //         n_head,
                //         n_past + n,
                //     ),
                //     1,
                //     2,
                //     0,
                //     3,
                // );

                // // GGML_ASSERT: ggml/ggml.c:4899: !ggml_is_transposed(a)
                // let k_q_v = ctx0.op_mul_mat(&v_trans, &k_q_soft_max);

                // split cached V into n_head heads
                let big_v = ctx0.op_view_3d(
                    &session.memory_v,
                    (n_past + n, n_embd / n_head, n_head),
                    (n_ctx * memv_elsize, n_ctx * memv_elsize * n_embd / n_head),
                    il * n_ctx * memv_elsize * n_embd,
                );

                // KQV = transpose(V) * KQ_soft_max
                let k_q_v = ctx0.op_mul_mat(&big_v, &k_q_soft_max);

                // KQV_merged = KQV.permute(0, 2, 1, 3)
                let k_q_v_merged = ctx0.op_permute(&k_q_v, 0, 2, 1, 3);

                // cur = KQV_merged.contiguous().view(n_embd, N)
                current = ctx0.op_cpy(
                    &k_q_v_merged,
                    &ctx0.new_tensor_2d(ggml::Type::F32, n_embd, n),
                );

                // projection
                current = ctx0.op_mul_mat(&self.layers[il].wo, &current);
                current = ctx0.op_add(&ctx0.op_repeat(&self.layers[il].wo_b, &current), &current);
            }

            let input_feed_forward = ctx0.op_add(&current, &input_self_attention);

            // feed-forward network
            {
                // norm
                {
                    current = ctx0.op_norm(&input_feed_forward);

                    // cur = ffn_norm*cur + ffn_norm_b
                    current = ctx0.op_mul(
                        &ctx0.op_repeat(&self.layers[il].ffn_norm, &current),
                        &current,
                    );

                    current = ctx0.op_add(
                        &ctx0.op_repeat(&self.layers[il].ffn_norm_b, &current),
                        &current,
                    );
                }

                current = ctx0.op_mul_mat(&self.layers[il].w1, &current);

                current = ctx0.op_add(&ctx0.op_repeat(&self.layers[il].w1_b, &current), &current);

                // SILU activation

                current = ctx0.op_gelu(&current);

                current = ctx0.op_mul_mat(&self.layers[il].w2, &current);

                current = ctx0.op_add(&ctx0.op_repeat(&self.layers[il].w2_b, &current), &current);
            }

            current = ctx0.op_add(&current, &input_feed_forward);

            // input for next layer
            input_layer = current;
        }

        // norm
        {
            input_layer = ctx0.op_norm(&input_layer);

            // inpL = norm*inpL
            input_layer = ctx0.op_mul(
                &ctx0.op_repeat(&self.output_norm, &input_layer),
                &input_layer,
            );

            input_layer = ctx0.op_add(
                &ctx0.op_repeat(&self.output_norm_b, &input_layer),
                &input_layer,
            );
<<<<<<< HEAD

            embeddings_tensor = input_layer.share();
=======
>>>>>>> main
        }

        // lm_head
        {
            input_layer = ctx0.op_mul_mat(&self.output, &input_layer);
        }

        // run the computation
        gf.build_forward_expand(&input_layer);
        ctx0.graph_compute(&mut gf);

        // finish evaluation
        common::read_last_token(session, &input_layer, n_vocab, n);
        common::extract_logits(output_request, &input_layer, n_vocab, n);
        common::extract_embeddings(output_request, &embd, n_embd, n);
        common::update_session(session, &ctx0, input_tokens.len(), n);
    }

    /// Returns the vocabulary used by this model.
    fn vocabulary(&self) -> &Vocabulary {
        &self.vocabulary
    }

    fn n_context_tokens(&self) -> usize {
        self.n_context_tokens
    }

    fn eot_token_id(&self) -> TokenId {
        0
    }

    fn inference_params(&self) -> InferenceParameters {
        self.inference_params.clone()
    }

    fn inference_prompt_params(&self) -> InferenceWithPromptParameters {
        self.inference_prompt_params
    }
}

/// BLOOM [hyperparameters](https://en.wikipedia.org/wiki/Hyperparameter_(machine_learning))
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct Hyperparameters {
    /// Size of the model's vocabulary
    pub n_vocab: usize,
    /// Size of the model's embedding layer
    pub n_embd: usize,
    /// n_mult
    pub n_mult: usize,
    /// n_head
    pub n_head: usize,
    /// Number of layers in the model
    pub n_layer: usize,
    /// file_type
    pub file_type: FileType,
}
impl llm_base::Hyperparameters for Hyperparameters {
    type WriteError = llm_base::BasicWriteError;

    fn read(reader: &mut dyn std::io::BufRead) -> Result<Self, llm_base::LoadError> {
        // NOTE: Field order matters! Data is laid out in the file exactly
        // in this order.
        Ok(Hyperparameters {
            n_vocab: util::read_i32(reader)?.try_into()?,
            n_embd: util::read_i32(reader)?.try_into()?,
            n_mult: util::read_i32(reader)?.try_into()?,
            n_head: util::read_i32(reader)?.try_into()?,
            n_layer: util::read_i32(reader)?.try_into()?,
            file_type: {
                let ftype = util::read_i32(reader)?;
                FileType::try_from(ftype).map_err(|_| LoadError::UnsupportedFileType(ftype))?
            },
        })
    }

    fn write(&self, writer: &mut dyn std::io::Write) -> Result<(), Self::WriteError> {
        util::write_i32(writer, self.n_vocab.try_into()?)?;
        util::write_i32(writer, self.n_embd.try_into()?)?;
        util::write_i32(writer, self.n_mult.try_into()?)?;
        util::write_i32(writer, self.n_head.try_into()?)?;
        util::write_i32(writer, self.n_layer.try_into()?)?;
        util::write_i32(writer, self.file_type.into())?;
        Ok(())
    }

    fn n_vocabulary(&self) -> usize {
        self.n_vocab
    }
}

struct Layer {
    pub attention_norm: Tensor,
    pub attention_norm_b: Tensor,
    pub wo: Tensor,
    pub wo_b: Tensor,
    pub query_key_value: Tensor,
    pub query_key_value_b: Tensor,
    // normalization
    pub ffn_norm: Tensor,
    pub ffn_norm_b: Tensor,
    // ff
    pub w1: Tensor,
    pub w1_b: Tensor,
    pub w2: Tensor,
    pub w2_b: Tensor,
}
