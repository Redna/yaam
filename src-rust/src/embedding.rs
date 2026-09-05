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

        let session = Session::builder()?.with_intra_threads(2)?.with_inter_threads(1)?.commit_from_file(model_path)?;

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

    /// Embed multiple texts in a single ONNX forward pass (batch inference).
    ///
    /// All texts are tokenized with padding to the longest sequence, then
    /// processed in one `session.run()` call. This is significantly faster
    /// than calling `embed()` N times because:
    /// - One kernel launch instead of N
    /// - Larger matrix multiplies are better parallelized by SIMD
    /// - Amortized session lock + tokenizer overhead
    ///
    /// Returns one L2-normalized 384-dim vector per input text, in order.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Dynamically adjust batch size based on available system memory
        let mut available_mb = 1024; // Default to 1GB if we can't read meminfo
        #[cfg(target_os = "linux")]
        {
            if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
                for line in meminfo.lines() {
                    if line.starts_with("MemAvailable:") {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() >= 2 {
                            if let Ok(kb) = parts[1].parse::<u64>() {
                                available_mb = kb / 1024;
                            }
                        }
                        break;
                    }
                }
            }
        }

        let batch_size_limit = if available_mb > 4000 {
            64
        } else if available_mb > 2000 {
            32
        } else if available_mb > 1000 {
            16
        } else if available_mb > 500 {
            8
        } else {
            2
        };

        if texts.len() > batch_size_limit {
            let mut all_results = Vec::with_capacity(texts.len());
            for chunk in texts.chunks(batch_size_limit) {
                let res = self.embed_batch(chunk)?;
                all_results.extend(res);
            }
            return Ok(all_results);
        }

        if texts.len() == 1 {
            return Ok(vec![self.embed(texts[0])?]);
        }

        // Batch tokenize with padding
        let encodings = self.tokenizer.encode_batch(texts.to_vec(), true)
            .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;

        let batch_size = encodings.len();
        let mut max_seq_len = encodings.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
        max_seq_len = std::cmp::min(max_seq_len, 512);
        if max_seq_len == 0 {
            return Ok(vec![vec![0.0f32; 384]; batch_size]); // fallback
        }

        // Flatten into (batch_size, max_seq_len) tensors
        let mut input_ids_flat = Vec::with_capacity(batch_size * max_seq_len);
        let mut attention_mask_flat = Vec::with_capacity(batch_size * max_seq_len);
        let mut token_type_ids_flat = Vec::with_capacity(batch_size * max_seq_len);
        let mut attention_masks: Vec<Vec<u32>> = Vec::with_capacity(batch_size);

        for enc in &encodings {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let type_ids = enc.get_type_ids();
            let seq_len = std::cmp::min(ids.len(), 512);

            input_ids_flat.extend(ids[..seq_len].iter().map(|&x| x as i64));
            attention_mask_flat.extend(mask[..seq_len].iter().map(|&x| x as i64));
            token_type_ids_flat.extend(type_ids[..seq_len].iter().map(|&x| x as i64));
            attention_masks.push(mask[..seq_len].to_vec());

            // Pad to max_seq_len if this sequence is shorter
            for _ in seq_len..max_seq_len {
                input_ids_flat.push(0);
                attention_mask_flat.push(0);
                token_type_ids_flat.push(0);
            }
        }

        let input_ids_tensor = Tensor::from_array(
            Array2::from_shape_vec((batch_size, max_seq_len), input_ids_flat)?
        )?;
        let attention_mask_tensor = Tensor::from_array(
            Array2::from_shape_vec((batch_size, max_seq_len), attention_mask_flat)?
        )?;
        let token_type_ids_tensor = Tensor::from_array(
            Array2::from_shape_vec((batch_size, max_seq_len), token_type_ids_flat)?
        )?;

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

        // Mean-pool each sequence using its own attention mask, then L2-normalize
        let mut results = Vec::with_capacity(batch_size);
        for i in 0..batch_size {
            let mask = &attention_masks[i];
            let mut pooled = vec![0.0f32; hidden_size];
            let mut mask_sum = 0.0f32;

            for j in 0..max_seq_len {
                if mask[j] == 1 {
                    let offset = (i * max_seq_len + j) * hidden_size;
                    for k in 0..hidden_size {
                        pooled[k] += output_data[offset + k];
                    }
                    mask_sum += 1.0;
                }
            }

            let mut norm_sq = 0.0f32;
            for k in 0..hidden_size {
                pooled[k] /= mask_sum.max(1e-9);
                norm_sq += pooled[k] * pooled[k];
            }
            let norm = norm_sq.sqrt().max(1e-9);
            for k in 0..hidden_size {
                pooled[k] /= norm;
            }

            results.push(pooled);
        }

        Ok(results)
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

        // Cap chunk count to prevent unbounded memory growth on massive session-dump files.
        // A single 500KB text could previously produce ~1,400 chunks. Capping to 16 limits 
        // the explosion while retaining the start of the text for semantic search.
        chunks.truncate(16);

        // Embed all chunks in a single batched ONNX forward pass
        let chunk_refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
        let embeddings = self.embed_batch(&chunk_refs)?;

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

pub fn encode_embeddings_base64(vectors: &[Vec<f32>]) -> serde_json::Value {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let b64_strings: Vec<String> = vectors.iter().map(|vec| {
        let bytes: Vec<u8> = vec.iter().flat_map(|&f| f.to_le_bytes()).collect();
        STANDARD.encode(&bytes)
    }).collect();
    serde_json::json!(b64_strings)
}

pub fn decode_embeddings_base64(val: &serde_json::Value) -> Option<Vec<Vec<f32>>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    
    // Support both old JSON float format and new base64 string format
    let outer = val.as_array()?;
    let mut result = Vec::new();
    
    for inner in outer {
        if let Some(s) = inner.as_str() {
            if let Ok(bytes) = STANDARD.decode(s) {
                let mut vec = Vec::with_capacity(bytes.len() / 4);
                for chunk in bytes.chunks_exact(4) {
                    vec.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
                result.push(vec);
            }
        } else if let Some(arr) = inner.as_array() {
            let vec: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            result.push(vec);
        }
    }
    
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}
