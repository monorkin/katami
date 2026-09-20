//! Static embeddings: semantic recall cheap enough for a hook path.
//!
//! Model2Vec models are lookup tables — embedding is tokenize, look up, and
//! mean-pool, microseconds on a CPU — so vectors can be computed inline
//! wherever a memory is written and queries can be embedded per keystroke of
//! budget. The model is fetched once with `katami memory pull-models`; until
//! then everything degrades to BM25-only, silently in hooks and with advice
//! in the CLI. Nothing ever downloads on a hook path.

use anyhow::Result;
use model2vec_rs::model::StaticModel;
use std::sync::OnceLock;

use crate::id::Id;
use crate::memory::Memory;
use crate::models::PinnedModel;

pub const MODEL_NAME: &str = "potion-base-8M";

const PINNED: PinnedModel = PinnedModel {
    name: MODEL_NAME,
    repo: "minishlab/potion-base-8M",
    files: &[
        (
            "config.json",
            "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
        ),
        (
            "tokenizer.json",
            "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
        ),
        (
            "model.safetensors",
            "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
        ),
    ],
};

static MODEL: OnceLock<Option<StaticModel>> = OnceLock::new();

pub fn available() -> bool {
    PINNED.available()
}

pub fn embed(text: &str) -> Option<Vec<f32>> {
    let model = MODEL.get_or_init(|| {
        if available() {
            StaticModel::from_pretrained(PINNED.directory(), None, None, None).ok()
        } else {
            None
        }
    });
    model.as_ref().map(|it| it.encode_single(text))
}

pub fn embed_into(memory: &Memory, id: Id, text: &str) -> Result<()> {
    if let Some(vector) = embed(text) {
        memory.set_embedding(id, MODEL_NAME, &vector)?;
    }
    Ok(())
}

pub fn pull() -> Result<()> {
    PINNED.pull()?;
    println!("Semantic search is ready — new memories are embedded automatically.");
    Ok(())
}
