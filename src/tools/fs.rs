//! The filesystem tool: read the target file into context, write the agent's edit back.
//!
//! v0.1 is deliberately narrow — the agent may touch exactly one path, given at
//! construction. Permissions and a broader tool surface are later releases.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::tools::workspace::Wrote;

/// A filesystem tool scoped to a single file.
#[derive(Debug, Clone)]
pub struct FsTool {
    path: PathBuf,
}

impl FsTool {
    /// Scope the tool to one file.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The path this tool operates on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the file into context. A missing file reads as empty, so the agent
    /// can create it.
    pub async fn read(&self) -> Result<String> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the agent's edit back to disk, creating parent directories,
    /// reporting whether it changed anything.
    ///
    /// The write happens in every case, [`Wrote::Unchanged`] included: this
    /// reports, it does not skip work.
    pub async fn write(&self, contents: &str) -> Result<Wrote> {
        let did = Wrote::classify(tokio::fs::read(&self.path).await, contents.as_bytes());
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        tokio::fs::write(&self.path, contents).await?;
        Ok(did)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_reads_empty_then_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FsTool::new(dir.path().join("nested/out.txt"));

        assert_eq!(tool.read().await.unwrap(), "");
        tool.write("hello").await.unwrap();
        assert_eq!(tool.read().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn a_write_reports_what_it_did_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FsTool::new(dir.path().join("nested/out.txt"));

        // Missing file: created.
        assert_eq!(tool.write("one").await.unwrap(), Wrote::Created);
        // Different content: changed, and the new content is on disk.
        assert_eq!(tool.write("two!").await.unwrap(), Wrote::Changed);
        assert_eq!(tool.read().await.unwrap(), "two!");
        // Byte-identical: unchanged.
        assert_eq!(tool.write("two!").await.unwrap(), Wrote::Unchanged);
        // Same length, different bytes: changed, not a length check.
        assert_eq!(tool.write("owt!").await.unwrap(), Wrote::Changed);
        assert_eq!(tool.read().await.unwrap(), "owt!");
        // Multibyte round-trips and compares by bytes, not chars.
        assert_eq!(tool.write("héllo — 日本語").await.unwrap(), Wrote::Changed);
        assert_eq!(tool.read().await.unwrap(), "héllo — 日本語");
        assert_eq!(
            tool.write("héllo — 日本語").await.unwrap(),
            Wrote::Unchanged
        );
    }
}
