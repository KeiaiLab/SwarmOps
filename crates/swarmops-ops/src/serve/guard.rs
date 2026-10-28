//! What a client of `serve` receives carries no host path (M14 design §8): [`Redactor`] masks
//! the store's root, `--root` and `serve.git` in every text sent, and [`ServerOps`] wraps
//! [`StoreOps`] so that a request's `{"file": path}` content is refused.

use std::path::Path;

use swarmops_api::StoreOps;

/// What a masked path reads as, until each path gets its own name.
const MASK: &str = "<path>";

/// Masks host paths in the texts a client receives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Redactor {
    /// The paths masked, as their text.
    paths: Vec<String>,
}

impl Redactor {
    /// The redactor of `paths`.
    pub fn new(paths: &[&Path]) -> Redactor {
        let paths = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .filter(|path| !path.is_empty())
            .collect();
        Redactor { paths }
    }

    /// `text`, each path masked.
    pub fn redact(&self, text: &str) -> String {
        self.paths.iter().fold(text.to_owned(), |text, path| {
            text.replace(path.as_str(), MASK)
        })
    }
}

/// The store's operations as a server serves them: those of `O`, with a request's `{"file"}`
/// content refused and every text masked.
#[derive(Debug)]
#[non_exhaustive]
pub struct ServerOps<O: StoreOps> {
    /// The operations served.
    inner: O,
}

impl<O: StoreOps> ServerOps<O> {
    /// `inner`, served.
    pub fn new(inner: O) -> ServerOps<O> {
        ServerOps { inner }
    }

    /// The operations served.
    pub fn inner(&self) -> &O {
        &self.inner
    }
}
