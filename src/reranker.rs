//! A relevance judge for retrieved memories.
//!
//! Search can only rank — ask it for five memories and it hands back five,
//! whether the prompt was "commit this" or "no dice". A cross-encoder reads
//! the prompt and one memory together and answers the question search can't:
//! is this memory about that prompt at all? Its raw logit is an absolute
//! score, so a floor can be drawn under it and a prompt that matches nothing
//! gets nothing injected.
//!
//! The model is a 23M-parameter MiniLM trained on MS MARCO, small enough to
//! judge a handful of candidates inside the hook's reply budget. It judges
//! topical relevance well; a memory that applies to a task without sharing
//! its words ("review before committing" against "commit it") scores as low
//! as noise. Closing that gap takes a model fine-tuned on this store's own
//! judgments, which is why every judgment is recorded.
//!
//! Each pair is judged on its own thread rather than as one padded batch:
//! memories vary a lot in length, so padding every pair out to the longest
//! one costs more than the batch saves, and a single short sequence is too
//! small for the matrix kernels to spread across cores by themselves.

use anyhow::{Context, Result, anyhow};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::{Linear, Module, VarBuilder};
use candle_transformers::models::bert::{BertModel, Config};
use std::sync::OnceLock;
use tokenizers::{Tokenizer, TruncationParams};

use crate::models::PinnedModel;

pub const MODEL_NAME: &str = "ms-marco-MiniLM-L6-v2";

const PINNED: PinnedModel = PinnedModel {
    name: MODEL_NAME,
    repo: "cross-encoder/ms-marco-MiniLM-L6-v2",
    files: &[
        (
            "config.json",
            "380e02c93f431831be65d99a4e7e5f67c133985bf2e77d9d4eba46847190bacc",
        ),
        (
            "tokenizer.json",
            "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66",
        ),
        (
            "model.safetensors",
            "821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae",
        ),
    ],
};

const MAX_TOKENS: usize = 256;

static MODEL: OnceLock<Reranker> = OnceLock::new();

struct Reranker {
    tokenizer: Tokenizer,
    encoder: BertModel,
    pooler: Linear,
    classifier: Linear,
}

pub fn available() -> bool {
    PINNED.available()
}

pub fn pull() -> Result<()> {
    PINNED.pull()?;
    println!("Relevance judging is ready — prompts that match nothing get nothing injected.");
    Ok(())
}

/// One raw logit per passage, in order; `None` until the model is pulled.
pub fn judge(query: &str, passages: &[String]) -> Result<Option<Vec<f32>>> {
    let Some(reranker) = loaded()? else {
        return Ok(None);
    };
    std::thread::scope(|scope| {
        let judging: Vec<_> = passages
            .iter()
            .map(|it| scope.spawn(move || reranker.logit(query, it)))
            .collect();
        judging
            .into_iter()
            .map(|it| it.join().expect("a judging thread panicked"))
            .collect::<Result<Vec<_>>>()
            .map(Some)
    })
}

fn loaded() -> Result<Option<&'static Reranker>> {
    if let Some(reranker) = MODEL.get() {
        Ok(Some(reranker))
    } else if available() {
        let reranker = Reranker::load()?;
        Ok(Some(MODEL.get_or_init(|| reranker)))
    } else {
        Ok(None)
    }
}

impl Reranker {
    fn load() -> Result<Reranker> {
        let directory = PINNED.directory();
        let config: Config = serde_json::from_slice(&std::fs::read(directory.join("config.json"))?)
            .context("the reranker's config.json did not parse")?;

        let mut tokenizer = Tokenizer::from_file(directory.join("tokenizer.json"))
            .map_err(|it| anyhow!("the reranker's tokenizer did not load: {it}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|it| anyhow!("the reranker's tokenizer refused truncation: {it}"))?;
        tokenizer.with_padding(None);

        let weights = std::fs::read(directory.join("model.safetensors"))?;
        let variables = VarBuilder::from_buffered_safetensors(weights, DType::F32, &Device::Cpu)?;
        Ok(Reranker {
            tokenizer,
            encoder: BertModel::load(variables.clone(), &config)?,
            pooler: candle_nn::linear(
                config.hidden_size,
                config.hidden_size,
                variables.pp("bert.pooler.dense"),
            )?,
            classifier: candle_nn::linear(config.hidden_size, 1, variables.pp("classifier"))?,
        })
    }

    fn logit(&self, query: &str, passage: &str) -> Result<f32> {
        let encoding = self
            .tokenizer
            .encode((query, passage), true)
            .map_err(|it| anyhow!("the reranker could not tokenize a pair: {it}"))?;
        let ids = Tensor::new(encoding.get_ids(), &Device::Cpu)?.unsqueeze(0)?;
        let segments = Tensor::new(encoding.get_type_ids(), &Device::Cpu)?.unsqueeze(0)?;

        let hidden = self.encoder.forward(&ids, &segments, None)?;
        let pooled = self.pooler.forward(&hidden.i((.., 0))?)?.tanh()?;
        let logit = self.classifier.forward(&pooled)?;
        Ok(logit.flatten_all()?.to_vec1::<f32>()?[0])
    }
}
