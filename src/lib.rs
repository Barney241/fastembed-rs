//! [FastEmbed](https://github.com/Anush008/fastembed-rs) - Fast, light, accurate library built for retrieval embedding generation.
//!
//! The library provides the TextEmbedding struct to interface with text embedding models.
//!
//! ### Instantiating [TextEmbedding](crate::TextEmbedding)
//! ```
//! use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};
//!
//!# fn model_demo() -> anyhow::Result<()> {
//! // With default InitOptions
//! let model = TextEmbedding::try_new(Default::default())?;
//!
//! // List all supported models
//! dbg!(TextEmbedding::list_supported_models());
//!
//! // With custom InitOptions
//! let model = TextEmbedding::try_new(InitOptions {
//!     model_name: EmbeddingModel::BGEBaseENV15,
//!     show_download_progress: false,
//!     ..Default::default()
//! })?;
//! # Ok(())
//! # }
//! ```
//! Find more info about the available options in the [InitOptions](crate::InitOptions) documentation.
//!
//! ### Embeddings generation
//!```
//!# use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};
//!# fn embedding_demo() -> anyhow::Result<()> {
//!# let model: TextEmbedding = TextEmbedding::try_new(Default::default())?;
//! let documents = vec![
//!    "passage: Hello, World!",
//!    "query: Hello, World!",
//!    "passage: This is an example passage.",
//!    // You can leave out the prefix but it's recommended
//!    "fastembed-rs is licensed under MIT"
//!    ];
//!
//! // Generate embeddings with the default batch size, 256
//! let embeddings = model.embed(documents, None)?;
//!
//! println!("Embeddings length: {}", embeddings.len()); // -> Embeddings length: 4
//! # Ok(())
//! # }
//! ```
//!

mod models;

#[cfg(test)]
mod tests;

use anyhow::{Ok, Result};
use hf_hub::{
    api::sync::{ApiBuilder, ApiRepo},
    Cache,
};
use models::models_list;
use ndarray::s;
use ndarray::Array;
use ort::{GraphOptimizationLevel, Session, Value};
use rayon::{iter::ParallelIterator, slice::ParallelSlice};
use std::{
    fmt::Display,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    thread::available_parallelism,
};
use tokenizers::{AddedToken, PaddingParams, PaddingStrategy, TruncationParams};

pub use ort::ExecutionProviderDispatch;

pub use crate::models::{EmbeddingModel, ModelInfo};

const DEFAULT_BATCH_SIZE: usize = 256;
const DEFAULT_MAX_LENGTH: usize = 512;
const DEFAULT_CACHE_DIR: &str = ".fastembed_cache";
const DEFAULT_EMBEDDING_MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;

/// Type alias for the embedding vector
pub type Embedding = Vec<f32>;

impl Display for EmbeddingModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let model_info = TextEmbedding::list_supported_models()
            .into_iter()
            .find(|model| model.model == *self)
            .unwrap();
        write!(f, "{}", model_info.model_code)
    }
}

/// Options for initializing the TextEmbedding model
#[derive(Debug, Clone)]
pub struct InitOptions {
    pub model_name: EmbeddingModel,
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    pub max_length: usize,
    pub cache_dir: PathBuf,
    pub show_download_progress: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            model_name: DEFAULT_EMBEDDING_MODEL,
            execution_providers: Default::default(),
            max_length: DEFAULT_MAX_LENGTH,
            cache_dir: Path::new(DEFAULT_CACHE_DIR).to_path_buf(),
            show_download_progress: true,
        }
    }
}

/// Options for initializing UserDefinedEmbeddingModel
///
/// Model files are held by the UserDefinedEmbeddingModel struct
#[derive(Debug, Clone)]
pub struct InitOptionsUserDefined {
    pub execution_providers: Vec<ExecutionProviderDispatch>,
    pub max_length: usize,
}

impl Default for InitOptionsUserDefined {
    fn default() -> Self {
        Self {
            execution_providers: Default::default(),
            max_length: DEFAULT_MAX_LENGTH,
        }
    }
}

/// Convert InitOptions to InitOptionsUserDefined
///
/// This is useful for when the user wants to use the same options for both the default and user-defined models
impl From<InitOptions> for InitOptionsUserDefined {
    fn from(options: InitOptions) -> Self {
        InitOptionsUserDefined {
            execution_providers: options.execution_providers,
            max_length: options.max_length,
        }
    }
}

/// Struct for "bring your own" embedding models
///
/// The onnx_file and tokenizer_files are expecting the files' bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDefinedEmbeddingModel {
    pub onnx_file: Vec<u8>,
    pub tokenizer_files: TokenizerFiles,
}

// Tokenizer files for "bring your own" embedding models
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizerFiles {
    pub tokenizer_file: Vec<u8>,
    pub config_file: Vec<u8>,
    pub special_tokens_map_file: Vec<u8>,
    pub tokenizer_config_file: Vec<u8>,
}

/// Rust representation of the TextEmbedding model
pub struct TextEmbedding {
    tokenizer: Tokenizer,
    session: Session,
    need_token_type_ids: bool,
}

impl TextEmbedding {
    /// Try to generate a new TextEmbedding Instance
    ///
    /// Uses the highest level of Graph optimization
    ///
    /// Uses the total number of CPUs available as the number of intra-threads
    pub fn try_new(options: InitOptions) -> Result<Self> {
        let InitOptions {
            model_name,
            execution_providers,
            max_length,
            cache_dir,
            show_download_progress,
        } = options;

        let threads = available_parallelism()?.get() as i16;

        let model_repo = TextEmbedding::retrieve_model(
            model_name.clone(),
            cache_dir.clone(),
            show_download_progress,
        )?;

        let model_file_name = TextEmbedding::get_model_info(&model_name).model_file;
        let model_file_reference = model_repo
            .get(&model_file_name)
            .unwrap_or_else(|_| panic!("Failed to retrieve {} ", model_file_name));

        // TODO: If more models need .onnx_data, implement a better way to handle this
        // Probably by adding `additonal_files` field in the `ModelInfo` struct
        if model_name == EmbeddingModel::MultilingualE5Large {
            model_repo
                .get("model.onnx_data")
                .expect("Failed to retrieve model.onnx_data.");
        }

        let session = Session::builder()?
            .with_execution_providers(execution_providers)?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?
            .with_model_from_file(model_file_reference)?;

        let tokenizer = TextEmbedding::load_tokenizer_hf_hub(model_repo, max_length)?;
        Ok(Self::new(tokenizer, session))
    }

    /// Create a TextEmbedding instance from model files provided by the user.
    ///
    /// This can be used for 'bring your own' embedding models
    pub fn try_new_from_user_defined(
        model: UserDefinedEmbeddingModel,
        options: InitOptionsUserDefined,
    ) -> Result<Self> {
        let InitOptionsUserDefined {
            execution_providers,
            max_length,
        } = options;

        let threads = available_parallelism()?.get() as i16;
        let session = Session::builder()?
            .with_execution_providers(execution_providers)?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?
            .with_model_from_memory(&model.onnx_file)?;

        let tokenizer = TextEmbedding::load_tokenizer(model.tokenizer_files, max_length)?;
        Ok(Self::new(tokenizer, session))
    }

    /// Private method to return an instance
    fn new(tokenizer: Tokenizer, session: Session) -> Self {
        let need_token_type_ids = session
            .inputs
            .iter()
            .any(|input| input.name == "token_type_ids");
        Self {
            tokenizer,
            session,
            need_token_type_ids,
        }
    }
    /// Return the TextEmbedding model's directory from cache or remote retrieval
    fn retrieve_model(
        model: EmbeddingModel,
        cache_dir: PathBuf,
        show_download_progress: bool,
    ) -> Result<ApiRepo> {
        let cache = Cache::new(cache_dir);
        let api = ApiBuilder::from_cache(cache)
            .with_progress(show_download_progress)
            .build()
            .unwrap();

        let repo = api.model(model.to_string());
        Ok(repo)
    }

    /// The procedure for loading tokenizer files from the hugging face hub is separated
    /// from the main load_tokenizer function (which is expecting bytes, from any source).
    fn load_tokenizer_hf_hub(model_repo: ApiRepo, max_length: usize) -> Result<Tokenizer> {
        let tokenizer_files: TokenizerFiles = TokenizerFiles {
            tokenizer_file: read_file_to_bytes(&model_repo.get("tokenizer.json")?)?,
            config_file: read_file_to_bytes(&model_repo.get("config.json")?)?,
            special_tokens_map_file: read_file_to_bytes(
                &model_repo.get("special_tokens_map.json")?,
            )?,

            tokenizer_config_file: read_file_to_bytes(&model_repo.get("tokenizer_config.json")?)?,
        };

        TextEmbedding::load_tokenizer(tokenizer_files, max_length)
    }

    /// Function can be called directly from the try_new_from_user_defined function (providing file bytes)
    ///
    /// Or indirectly from the try_new function via load_tokenizer_hf_hub (converting HF files to bytes)
    fn load_tokenizer(tokenizer_files: TokenizerFiles, max_length: usize) -> Result<Tokenizer> {
        let base_error_message =
            "Error building TokenizerFiles for UserDefinedEmbeddingModel. Could not read {} file.";

        // Serialise each tokenizer file
        let config: serde_json::Value = serde_json::from_slice(&tokenizer_files.config_file)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    base_error_message.replace("{}", "config.json"),
                )
            })?;
        let special_tokens_map: serde_json::Value =
            serde_json::from_slice(&tokenizer_files.special_tokens_map_file).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    base_error_message.replace("{}", "special_tokens_map.json"),
                )
            })?;
        let tokenizer_config: serde_json::Value =
            serde_json::from_slice(&tokenizer_files.tokenizer_config_file).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    base_error_message.replace("{}", "tokenizer_config.json"),
                )
            })?;
        let mut tokenizer: tokenizers::Tokenizer =
            tokenizers::Tokenizer::from_bytes(tokenizer_files.tokenizer_file).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    base_error_message.replace("{}", "tokenizer.json"),
                )
            })?;

        //For BGEBaseSmall, the model_max_length value is set to 1000000000000000019884624838656. Which fits in a f64
        let model_max_length = tokenizer_config["model_max_length"]
            .as_f64()
            .expect("Error reading model_max_length from tokenizer_config.json")
            as f32;
        let max_length = max_length.min(model_max_length as usize);
        let pad_id = config["pad_token_id"].as_u64().unwrap_or(0) as u32;
        let pad_token = tokenizer_config["pad_token"]
            .as_str()
            .expect("Error reading pad_token from tokenier_config.json")
            .into();

        let mut tokenizer = tokenizer
            .with_padding(Some(PaddingParams {
                // TODO: the user should able to choose the padding strategy
                strategy: PaddingStrategy::BatchLongest,
                pad_token,
                pad_id,
                ..Default::default()
            }))
            .with_truncation(Some(TruncationParams {
                max_length,
                ..Default::default()
            }))
            .map_err(anyhow::Error::msg)?
            .clone();
        if let serde_json::Value::Object(root_object) = special_tokens_map {
            for (_, value) in root_object.iter() {
                if value.is_string() {
                    tokenizer.add_special_tokens(&[AddedToken {
                        content: value.as_str().unwrap().into(),
                        special: true,
                        ..Default::default()
                    }]);
                } else if value.is_object() {
                    tokenizer.add_special_tokens(&[AddedToken {
                        content: value["content"].as_str().unwrap().into(),
                        special: true,
                        single_word: value["single_word"].as_bool().unwrap(),
                        lstrip: value["lstrip"].as_bool().unwrap(),
                        rstrip: value["rstrip"].as_bool().unwrap(),
                        normalized: value["normalized"].as_bool().unwrap(),
                    }]);
                }
            }
        }
        Ok(tokenizer)
    }

    /// Retrieve a list of supported models
    pub fn list_supported_models() -> Vec<ModelInfo> {
        models_list()
    }

    /// Get ModelInfo from EmbeddingModel
    pub fn get_model_info(model: &EmbeddingModel) -> ModelInfo {
        TextEmbedding::list_supported_models()
            .into_iter()
            .find(|m| &m.model == model)
            .expect("Model not found.")
    }

    /// Method to generate sentence embeddings for a Vec of texts
    // Generic type to accept String, &str, OsString, &OsStr
    pub fn embed<S: AsRef<str> + Send + Sync>(
        &self,
        texts: Vec<S>,
        batch_size: Option<usize>,
    ) -> Result<Vec<Embedding>> {
        // Determine the batch size, default if not specified
        let batch_size = batch_size.unwrap_or(DEFAULT_BATCH_SIZE);

        let output = texts
            .par_chunks(batch_size)
            .map(|batch| {
                // Encode the texts in the batch
                let inputs = batch.iter().map(|text| text.as_ref()).collect();
                let encodings = self.tokenizer.encode_batch(inputs, true).unwrap();

                // Extract the encoding length and batch size
                let encoding_length = encodings[0].len();
                let batch_size = batch.len();

                let max_size = encoding_length * batch_size;

                // Preallocate arrays with the maximum size
                let mut ids_array = Vec::with_capacity(max_size);
                let mut mask_array = Vec::with_capacity(max_size);
                let mut typeids_array = Vec::with_capacity(max_size);

                // Not using par_iter because the closure needs to be FnMut
                encodings.iter().for_each(|encoding| {
                    let ids = encoding.get_ids();
                    let mask = encoding.get_attention_mask();
                    let typeids = encoding.get_type_ids();

                    // Extend the preallocated arrays with the current encoding
                    // Requires the closure to be FnMut
                    ids_array.extend(ids.iter().map(|x| *x as i64));
                    mask_array.extend(mask.iter().map(|x| *x as i64));
                    typeids_array.extend(typeids.iter().map(|x| *x as i64));
                });

                // Create CowArrays from vectors
                let inputs_ids_array =
                    Array::from_shape_vec((batch_size, encoding_length), ids_array)?;

                let attention_mask_array =
                    Array::from_shape_vec((batch_size, encoding_length), mask_array)?;

                let token_type_ids_array =
                    Array::from_shape_vec((batch_size, encoding_length), typeids_array)?;

                let mut session_inputs = ort::inputs![
                    "input_ids" => Value::from_array(inputs_ids_array)?,
                    "attention_mask" => Value::from_array(attention_mask_array)?,
                ]?;
                if self.need_token_type_ids {
                    session_inputs
                        .insert("token_type_ids", Value::from_array(token_type_ids_array)?);
                }

                let outputs = self.session.run(session_inputs)?;

                // Extract and normalize embeddings
                let output_data = outputs["last_hidden_state"].extract_tensor::<f32>()?;

                let embeddings: Vec<Vec<f32>> = output_data
                    .view()
                    .slice(s![.., 0, ..])
                    .rows()
                    .into_iter()
                    .map(|row| normalize(row.as_slice().unwrap()))
                    .collect();

                Ok(embeddings)
            })
            .flat_map(|result| result.unwrap())
            .collect();

        Ok(output)
    }
}

// This type was inferred using IDE hints
// Turned into a type alias for type hinting
type Tokenizer = tokenizers::TokenizerImpl<
    tokenizers::ModelWrapper,
    tokenizers::NormalizerWrapper,
    tokenizers::PreTokenizerWrapper,
    tokenizers::PostProcessorWrapper,
    tokenizers::DecoderWrapper,
>;

fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = (v.iter().map(|val| val * val).sum::<f32>()).sqrt();
    let epsilon = 1e-12;

    // We add the super-small epsilon to avoid dividing by zero
    v.iter().map(|&val| val / (norm + epsilon)).collect()
}

/// Read a file to bytes.
///
/// Could be used to read the onnx file from a local cache in order to constitute a UserDefinedEmbeddingModel.
pub fn read_file_to_bytes(file: &PathBuf) -> Result<Vec<u8>> {
    let mut file = File::open(file)?;
    let file_size = file.metadata()?.len() as usize;
    let mut buffer = Vec::with_capacity(file_size);
    file.read_to_end(&mut buffer)?;
    Ok(buffer)
}
