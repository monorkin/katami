//! Static embeddings: semantic recall cheap enough for a hook path.
//!
//! Model2Vec models are lookup tables — embedding is tokenize, look up, and
//! mean-pool, microseconds on a CPU — so vectors can be computed inline
//! wherever a memory is written and queries can be embedded per keystroke of
//! budget. The model is fetched once with `agent memory pull-models`; until
//! then everything degrades to BM25-only, silently in hooks and with advice
//! in the CLI. Nothing ever downloads on a hook path.

use anyhow::{Context, Result};
use model2vec_rs::model::StaticModel;
use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::memory::Memory;
use crate::paths;

pub const MODEL_NAME: &str = "potion-base-8M";
const MODEL_REPO: &str = "minishlab/potion-base-8M";
const MODEL_FILES: [(&str, &str); 3] = [
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
];

static MODEL: OnceLock<Option<StaticModel>> = OnceLock::new();

pub fn available() -> bool {
    MODEL_FILES
        .iter()
        .all(|(file, _)| model_dir().join(file).exists())
}

pub fn embed(text: &str) -> Option<Vec<f32>> {
    let model = MODEL.get_or_init(|| {
        if available() {
            StaticModel::from_pretrained(model_dir(), None, None, None).ok()
        } else {
            None
        }
    });
    model.as_ref().map(|it| it.encode_single(text))
}

pub fn embed_into(memory: &Memory, id: i64, text: &str) -> Result<()> {
    if let Some(vector) = embed(text) {
        memory.set_embedding(id, MODEL_NAME, &vector)?;
    }
    Ok(())
}

pub fn pull() -> Result<()> {
    let directory = model_dir();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;

    for (file, expected_sha256) in MODEL_FILES {
        let url = format!("https://huggingface.co/{MODEL_REPO}/resolve/main/{file}");
        println!("Fetching {file}…");

        let mut response = ureq::get(&url)
            .call()
            .with_context(|| format!("could not download {url}"))?;
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .read_to_end(&mut bytes)
            .with_context(|| format!("could not download {url}"))?;

        let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
        let actual: String = digest.as_ref().iter().map(|it| format!("{it:02x}")).collect();
        if actual != expected_sha256 {
            anyhow::bail!(
                "{file} from {MODEL_REPO} doesn't match its pinned checksum — the upstream model changed, refusing it"
            );
        }

        let temporary = directory.join(format!("{file}.tmp"));
        std::fs::write(&temporary, &bytes)?;
        std::fs::rename(&temporary, directory.join(file))?;
    }
    println!("Semantic search is ready — new memories are embedded automatically.");
    Ok(())
}

fn model_dir() -> PathBuf {
    paths::models_dir().join(MODEL_NAME)
}
