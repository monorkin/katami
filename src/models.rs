//! Models are fetched once, by hand, and pinned by checksum.
//!
//! Everything that runs on a hook path has to work offline and in
//! milliseconds, so nothing ever downloads there — a model is either on disk
//! or the feature it powers quietly stays off. `katami memory pull-models` is
//! the one place the network is touched, and a file that doesn't match its
//! pinned SHA-256 is refused rather than trusted: a model that changed
//! upstream would change what gets injected into every session.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::PathBuf;

use crate::paths;

pub struct PinnedModel {
    pub name: &'static str,
    pub repo: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

impl PinnedModel {
    pub fn directory(&self) -> PathBuf {
        paths::models_dir().join(self.name)
    }

    pub fn available(&self) -> bool {
        self.files
            .iter()
            .all(|(file, _)| self.directory().join(file).exists())
    }

    pub fn pull(&self) -> Result<()> {
        let directory = self.directory();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;

        for (file, expected_sha256) in self.files {
            let url = format!("https://huggingface.co/{}/resolve/main/{file}", self.repo);
            println!("Fetching {} {file}…", self.name);

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
            if actual != *expected_sha256 {
                anyhow::bail!(
                    "{file} from {} doesn't match its pinned checksum — the upstream model changed, refusing it",
                    self.repo
                );
            }

            let temporary = directory.join(format!("{file}.tmp"));
            std::fs::write(&temporary, &bytes)?;
            std::fs::rename(&temporary, directory.join(file))?;
        }
        Ok(())
    }
}
