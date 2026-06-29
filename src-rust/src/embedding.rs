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

        tokenizer.with_truncation(Some(TruncationParams {
            max_length: 512,
            ..Default::default()
        }));

        tokenizer.with_padding(Some(PaddingParams {
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
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
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
