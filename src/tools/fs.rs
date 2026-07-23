//! The filesystem tool: read the target file into context, write the agent's edit back.
//!
//! v0.1 is deliberately narrow — the agent may touch exactly one path, given at
//! construction. Permissions and a broader tool surface are later releases.

use std::path::{Path, PathBuf};

use crate::error::Result;

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

    /// Write the agent's edit back to disk, creating parent directories.
    pub async fn write(&self, contents: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        tokio::fs::write(&self.path, contents).await?;
        Ok(())
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
}
