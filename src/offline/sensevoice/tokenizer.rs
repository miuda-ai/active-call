use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{anyhow, Context, Result};

pub struct TokenDecoder {
    pieces: Vec<String>,
}

impl TokenDecoder {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())
            .with_context(|| format!("open tokens file {}", path.as_ref().display()))?;
        let mut pieces: Vec<String> = Vec::new();
        for (line_idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line.with_context(|| format!("read line {}", line_idx + 1))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (token, id_str) = trimmed
                .rsplit_once(' ')
                .ok_or_else(|| anyhow!("invalid tokens entry: '{}'", trimmed))?;
            let id: usize = id_str
                .parse()
                .with_context(|| format!("parse token id at line {}", line_idx + 1))?;
            if id >= pieces.len() {
                pieces.resize(id + 1, String::new());
            }
            pieces[id] = token.to_owned();
        }
        if pieces.is_empty() {
            anyhow::bail!("tokens list is empty");
        }
        for (idx, piece) in pieces.iter().enumerate() {
            if piece.is_empty() {
                anyhow::bail!("missing token for id {}", idx);
            }
        }
        Ok(Self { pieces })
    }

    pub fn decode_ids(&self, ids: &[i32]) -> String {
        let mut text = String::new();
        for &id in ids {
            if id < 0 {
                continue;
            }
            let idx = id as usize;
            if idx >= self.pieces.len() {
                continue;
            }
            let piece = &self.pieces[idx];
            if piece == "<unk>" || piece == "<s>" || piece == "</s>" {
                continue;
            }
            if piece.starts_with('<') && piece.ends_with('>') {
                continue;
            }
            if let Some(stripped) = piece.strip_prefix('▁') {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(stripped);
            } else {
                text.push_str(piece);
            }
        }
        text.trim().to_string()
    }
}
