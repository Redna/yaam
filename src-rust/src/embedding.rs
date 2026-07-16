use ndarray::Array2;
use ort::session::Session;
use ort::value::Tensor;
use std::sync::Mutex;
use std::path::Path;
use tokenizers::{PaddingParams, TruncationParams, Tokenizer};

pub struct EmbeddingModel {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl EmbeddingModel {
    pub fn new(model_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let model_path = model_dir.join("model.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");

        let session = Session::builder()?.commit_from_file(model_path)?;

        let mut tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;

        let _ = tokenizer.with_truncation(Some(TruncationParams {
            max_length: 512,
            ..Default::default()
        }));

        let _ = tokenizer.with_padding(Some(PaddingParams {
            ..Default::default()
        }));

        Ok(Self { session: Mutex::new(session), tokenizer })
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let encoding = self.tokenizer.encode(text, true).map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;

        let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
        let attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&x| x as i64)
            .collect();
        let token_type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&x| x as i64).collect();

        let seq_len = input_ids.len();

        let input_ids_tensor = Tensor::from_array(Array2::from_shape_vec((1, seq_len), input_ids)?)?;
        let attention_mask_tensor = Tensor::from_array(Array2::from_shape_vec((1, seq_len), attention_mask)?)?;
        let token_type_ids_tensor = Tensor::from_array(Array2::from_shape_vec((1, seq_len), token_type_ids)?)?;

        let mut session_guard = self.session.lock().unwrap();
        let outputs = if session_guard.inputs().iter().any(|i| i.name() == "token_type_ids") {
            session_guard.run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            ])?
        } else {
            session_guard.run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
            ])?
        };

        let output_tensor = outputs[0].try_extract_tensor::<f32>()?;
        let output_shape = output_tensor.0;
        let output_data = output_tensor.1;
        let hidden_size = output_shape[2] as usize;

        let mut pooled = vec![0.0f32; hidden_size];
        let mut mask_sum = 0.0f32;

        let attention_mask_slice = encoding.get_attention_mask();

        for i in 0..seq_len {
            if attention_mask_slice[i] == 1 {
                for j in 0..hidden_size {
                    pooled[j] += output_data[i * hidden_size + j];
                }
                mask_sum += 1.0;
            }
        }

        let mut norm_sq = 0.0f32;
        for j in 0..hidden_size {
            pooled[j] /= mask_sum.max(1e-9);
            norm_sq += pooled[j] * pooled[j];
        }

        let norm = norm_sq.sqrt().max(1e-9);
        for j in 0..hidden_size {
            pooled[j] /= norm;
        }

        Ok(pooled)
    }

    /// Count the number of tokens in `text` using the tokenizer.
    /// Used by `embed_chunked` to determine chunk boundaries.
    pub fn count_tokens(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map(|enc| enc.get_ids().len())
            .unwrap_or(0)
    }

    /// Embed text, splitting into overlapping chunks if it exceeds `max_tokens`.
    ///
    /// Returns a list of embedding vectors — one per chunk. Short text returns
    /// a single-element list. Long text is split on paragraph boundaries first,
    /// then sentence boundaries, with `overlap_tokens` of overlap between
    /// consecutive chunks.
    pub fn embed_chunked(
        &self,
        text: &str,
        max_tokens: usize,
        overlap_tokens: usize,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        let total_tokens = self.count_tokens(text);
        if total_tokens <= max_tokens {
            return Ok(vec![self.embed(text)?]);
        }

        // Split into paragraphs first
        let paragraphs: Vec<&str> = text.split("\n\n").collect();

        // Group paragraphs into chunks that fit within max_tokens
        let mut chunks: Vec<String> = Vec::new();
        let mut current_chunk = String::new();
        let mut current_tokens = 0usize;

        for para in &paragraphs {
            let para_tokens = self.count_tokens(para);

            // If a single paragraph exceeds max_tokens, split it on sentences
            if para_tokens > max_tokens && current_chunk.is_empty() {
                let sentences = split_sentences(para);
                let mut sent_chunk = String::new();
                let mut sent_tokens = 0usize;
                for sent in sentences {
                    let st = self.count_tokens(&sent);
                    if sent_tokens + st > max_tokens && !sent_chunk.is_empty() {
                        chunks.push(sent_chunk.clone());
                        sent_chunk = overlap_text(&chunks, overlap_tokens);
                        sent_tokens = self.count_tokens(&sent_chunk);
                    }
                    if !sent_chunk.is_empty() {
                        sent_chunk.push(' ');
                    }
                    sent_chunk.push_str(&sent);
                    sent_tokens += st;
                }
                if !sent_chunk.is_empty() {
                    chunks.push(sent_chunk);
                    current_chunk = overlap_text(&chunks, overlap_tokens);
                    current_tokens = self.count_tokens(&current_chunk);
                }
                continue;
            }

            // Check if adding this paragraph would exceed the limit
            if current_tokens + para_tokens > max_tokens && !current_chunk.is_empty() {
                chunks.push(current_chunk.clone());
                // Start next chunk with overlap from the end of the previous chunk
                current_chunk = overlap_text(&chunks, overlap_tokens);
                current_tokens = self.count_tokens(&current_chunk);
            }

            if !current_chunk.is_empty() {
                current_chunk.push_str("\n\n");
            }
            current_chunk.push_str(para);
            current_tokens += para_tokens;
        }

        if !current_chunk.is_empty() {
            chunks.push(current_chunk);
        }

        // Embed each chunk
        let mut embeddings = Vec::new();
        for chunk in &chunks {
            embeddings.push(self.embed(chunk)?);
        }

        Ok(embeddings)
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Split text into sentences on `. `, `! `, or `? ` boundaries.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        current.push(c);
        if (c == '.' || c == '!' || c == '?') {
            // Include trailing space if present
            sentences.push(current.trim().to_string());
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }
    sentences
}

/// Extract approximately `overlap_tokens` worth of text from the end of the
/// last chunk to use as overlap for the next chunk.
/// This is a rough heuristic — it takes the last few sentences.
fn overlap_text(chunks: &[String], overlap_tokens: usize) -> String {
    if chunks.is_empty() || overlap_tokens == 0 {
        return String::new();
    }
    let last = chunks.last().unwrap();
    let sentences = split_sentences(last);
    // Take sentences from the end until we have enough overlap
    let mut overlap = String::new();
    let mut approx_tokens = 0;
    // Rough estimate: ~1.3 words per token, ~5 words per sentence
    let target_sentences = (overlap_tokens / 7).max(1);
    for sent in sentences.iter().rev().take(target_sentences) {
        if !overlap.is_empty() {
            overlap = format!("{} {}", sent, overlap);
        } else {
            overlap = sent.clone();
        }
        approx_tokens += sent.split_whitespace().count();
    }
    let _ = approx_tokens; // suppress unused warning
    overlap
}

pub fn decay_weight(created_at: u64, current_time: u64) -> f32 {
    if current_time < created_at {
        return 1.0;
    }
    let age_in_hours = (current_time - created_at) as f32 / 3600.0;
    (-0.05 * age_in_hours).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);

        let a = [1.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 1.0);

        let a = [0.8, 0.6];
        let b = [0.8, 0.6];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_decay_weight() {
        let created_at = 10000;
        let current_time = 10000;
        assert_eq!(decay_weight(created_at, current_time), 1.0);

        let current_time_future = 10000 + 3600; // 1 hour
        let w = decay_weight(created_at, current_time_future);
        assert!((w - (-0.05f32).exp()).abs() < 1e-6);

        let current_time_past = 9000;
    }
}

pub async fn download_model_files() -> Result<(), Box<dyn std::error::Error>> {
    let model_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()))
        .join(".yaam").join("models");
    
    if !model_dir.exists() {
        std::fs::create_dir_all(&model_dir)?;
    }

    let files = [
        ("model.onnx", "https://huggingface.co/Xenova/gte-small/resolve/main/onnx/model.onnx"),
        ("tokenizer.json", "https://huggingface.co/Xenova/gte-small/resolve/main/tokenizer.json"),
    ];

    for (filename, url) in files.iter() {
        let file_path = model_dir.join(filename);
        if !file_path.exists() {
            println!("Downloading {}...", filename);
            let response = reqwest::get(*url).await?.error_for_status()?;
            let bytes = response.bytes().await?;
            std::fs::write(&file_path, bytes)?;
            println!("Downloaded {}.", filename);
        }
    }

    Ok(())
}
