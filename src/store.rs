//! 索引持久化：JSON 读写。

use crate::model::Index;
use std::path::Path;

/// 保存索引到 JSON 文件。
pub fn save(index: &Index, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, json)
}

/// 从 JSON 文件加载索引。
pub fn load(path: &Path) -> std::io::Result<Index> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
