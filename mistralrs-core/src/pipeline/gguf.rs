use super::{
    calculate_inputs, get_model_paths, get_xlora_paths, Loader, ModelInputs, ModelKind, ModelPaths,
    Pipeline, TokenSource, XLoraPaths,
};
use crate::aici::bintokens::build_tok_trie;
use crate::aici::toktree::TokTrie;
use crate::models::Cache;
use crate::pipeline::{calculate_eos_tok, ChatTemplate};
use crate::utils::varbuilder_utils::from_mmaped_safetensors;
use crate::xlora_models::{NonGranularState, XLoraConfig};
use crate::{deserialize_chat_template, get_paths};
use crate::{
    models::quantized_llama::ModelWeights as QLlama, models::quantized_phi2::ModelWeights as QPhi,
    sequence::Sequence, utils::tokens::get_token, xlora_models::XLoraModelWeights as XLoraQLlama,
};
use anyhow::{bail, Result};
use candle_core::quantized::gguf_file;
use candle_core::{DType, Device, Tensor};
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use mistralrs_lora::{LoraConfig, Ordering};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;
use thiserror::Error;
use tokenizers::Tokenizer;
use tracing::info;

enum Model {
    Llama(QLlama),
    Phi2(QPhi),
    XLoraLlama(XLoraQLlama),
}

pub struct MistralModelPaths<P> {
    tokenizer_filename: P,
    config_filename: P,
    template_filename: P,
    filenames: Vec<P>,
    xlora_adapter_filenames: Option<Vec<(String, P)>>,
    xlora_adapter_configs: Option<Vec<(String, LoraConfig)>>,
    classifier_path: Option<P>,
    classifier_config: Option<XLoraConfig>,
    xlora_ordering: Option<Ordering>,
}

impl ModelPaths for MistralModelPaths<PathBuf> {
    fn get_config_filename(&self) -> &PathBuf {
        &self.config_filename
    }
    fn get_tokenizer_filename(&self) -> &PathBuf {
        &self.tokenizer_filename
    }
    fn get_weight_filenames(&self) -> &[PathBuf] {
        &self.filenames
    }
    fn get_adapter_filenames(&self) -> &Option<Vec<(String, PathBuf)>> {
        &self.xlora_adapter_filenames
    }
    fn get_adapter_configs(&self) -> &Option<Vec<(String, LoraConfig)>> {
        &self.xlora_adapter_configs
    }
    fn get_classifier_config(&self) -> &Option<XLoraConfig> {
        &self.classifier_config
    }
    fn get_classifier_path(&self) -> &Option<PathBuf> {
        &self.classifier_path
    }
    fn get_ordering(&self) -> &Option<Ordering> {
        &self.xlora_ordering
    }
    fn get_template_filename(&self) -> &PathBuf {
        &self.template_filename
    }
}

pub struct GgufPipeline {
    model: Model,
    config: GgufSpecificConfig,
    tokenizer: Arc<Tokenizer>,
    tok_trie: TokTrie,
    no_kv_cache: bool,
    chat_template: ChatTemplate,
    model_id: String,
    eos_tok: Vec<u32>,
    non_granular_state: Option<NonGranularState>,
    is_lora: bool,
}

pub struct GgufLoader {
    model_id: String,
    config: GgufSpecificConfig,
    quantized_model_id: Option<String>,
    quantized_filename: Option<String>,
    xlora_model_id: Option<String>,
    xlora_order: Option<Ordering>,
    no_kv_cache: bool,
    chat_template: Option<String>,
    tokenizer_json: Option<String>,
    kind: ModelKind,
    tgt_non_granular_index: Option<usize>,
}

#[derive(Debug)]
enum GgufArchitecture {
    Llama,
    Mpt,
    Gptneox,
    Gptj,
    Gpt2,
    Bloom,
    Falcon,
    Mamba,
    Rwkv,
    Phi2,
}

impl FromStr for GgufArchitecture {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "llama" => Ok(GgufArchitecture::Llama),
            "mpt" => Ok(GgufArchitecture::Mpt),
            "gptneox" => Ok(GgufArchitecture::Gptneox),
            "gptj" => Ok(GgufArchitecture::Gptj),
            "gpt2" => Ok(GgufArchitecture::Gpt2),
            "bloom" => Ok(GgufArchitecture::Bloom),
            "falcon" => Ok(GgufArchitecture::Falcon),
            "mamba" => Ok(GgufArchitecture::Mamba),
            "rwkv" => Ok(GgufArchitecture::Rwkv),
            "phi2" => Ok(GgufArchitecture::Phi2),
            a => Err(format!("Unknown GGUF architecture `{a}`")),
        }
    }
}

#[derive(Clone, Copy)]
pub struct GgufSpecificConfig {
    pub repeat_last_n: usize,
}

#[derive(Error, Debug)]
enum TokenizerError {
    #[error("`{0}`")]
    Error(String),
}

impl GgufLoader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: Option<String>,
        config: GgufSpecificConfig,
        quantized_model_id: Option<String>,
        quantized_filename: Option<String>,
        xlora_model_id: Option<String>,
        kind: ModelKind,
        xlora_order: Option<Ordering>,
        no_kv_cache: bool,
        chat_template: Option<String>,
        tokenizer_json: Option<String>,
        tgt_non_granular_index: Option<usize>,
    ) -> Self {
        let model_id = if let Some(id) = model_id {
            id
        } else {
            info!(
                "Using adapter base model ID: `{}`",
                xlora_order.as_ref().unwrap().base_model_id
            );
            xlora_order.as_ref().unwrap().base_model_id.clone()
        };
        Self {
            model_id,
            config,
            quantized_model_id,
            quantized_filename,
            xlora_model_id,
            xlora_order,
            no_kv_cache,
            chat_template,
            tokenizer_json,
            kind,
            tgt_non_granular_index,
        }
    }
}

impl Loader for GgufLoader {
    fn download_model(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
    ) -> Result<Box<dyn ModelPaths>> {
        get_paths!(
            MistralModelPaths,
            &token_source,
            revision,
            self,
            self.quantized_model_id,
            self.quantized_filename
        )
    }

    fn _setup_model(
        &self,
        paths: &dyn ModelPaths,
        dtype: Option<DType>,
        device: &Device,
    ) -> Result<Box<Mutex<dyn Pipeline + Send + Sync>>> {
        let default_dtype = if device.is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };

        let mut file = std::fs::File::open(paths.get_weight_filenames().first().unwrap())?;
        let model = gguf_file::Content::read(&mut file)
            .map_err(|e| e.with_path(paths.get_weight_filenames().first().unwrap()))?;
        let arch: GgufArchitecture = model.metadata["general.architecture"]
            .to_string()
            .unwrap()
            .parse()
            .map_err(anyhow::Error::msg)?;

        let mut is_lora = false;
        let model = match self.kind {
            ModelKind::QuantizedGGUF => match arch {
                GgufArchitecture::Llama => {
                    Model::Llama(QLlama::from_gguf(model, &mut file, device)?)
                }
                GgufArchitecture::Phi2 => Model::Phi2(QPhi::from_gguf(model, &mut file, device)?),
                a => bail!("Unsupported architecture `{a:?}`"),
            },
            ModelKind::XLoraGGUF => {
                let vb = from_mmaped_safetensors(
                    vec![paths.get_classifier_path().as_ref().unwrap().to_path_buf()],
                    paths
                        .get_adapter_filenames()
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|(_, x)| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                match arch {
                    GgufArchitecture::Llama => Model::XLoraLlama(XLoraQLlama::from_gguf(
                        model,
                        &mut file,
                        device,
                        paths.get_adapter_configs().as_ref().unwrap(),
                        &vb,
                        paths.get_ordering().as_ref().unwrap(),
                        Some(paths.get_classifier_config().as_ref().unwrap().clone()),
                    )?),
                    a => bail!("Unsupported architecture for GGUF X-LoRA `{a:?}`"),
                }
            }
            ModelKind::LoraGGUF => {
                is_lora = true;
                let vb = from_mmaped_safetensors(
                    vec![],
                    paths
                        .get_adapter_filenames()
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|(_, x)| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                match arch {
                    GgufArchitecture::Llama => Model::XLoraLlama(XLoraQLlama::from_gguf(
                        model,
                        &mut file,
                        device,
                        paths.get_adapter_configs().as_ref().unwrap(),
                        &vb,
                        paths.get_ordering().as_ref().unwrap(),
                        Some(paths.get_classifier_config().as_ref().unwrap().clone()),
                    )?),
                    a => bail!("Unsupported architecture for GGUF X-LoRA `{a:?}`"),
                }
            }
            _ => unreachable!(),
        };

        let tokenizer = Tokenizer::from_file(paths.get_tokenizer_filename())
            .map_err(|e| TokenizerError::Error(e.to_string()))?;

        let chat_template: ChatTemplate = deserialize_chat_template!(paths, self);
        let mut eos_toks = vec![chat_template.eos_tok()];

        // Handle Llama3 chat case
        if tokenizer.get_vocab(true).get("<|eot_id|>").is_some() {
            eos_toks.push("<|eot_id|>".to_string())
        }

        info!(
            "bos_tok = {}, eos_tok = {:?}, unk_tok = {}",
            chat_template.bos_tok(),
            eos_toks,
            chat_template.eos_tok()
        );

        Ok(Box::new(Mutex::new(GgufPipeline {
            model,
            config: self.config,
            eos_tok: calculate_eos_tok(eos_toks, &tokenizer),
            tok_trie: build_tok_trie(tokenizer.clone()),
            tokenizer: tokenizer.into(),
            no_kv_cache: self.no_kv_cache,
            chat_template,
            model_id: self.model_id.clone(),
            non_granular_state: self.tgt_non_granular_index.map(|tgt_non_granular_index| {
                NonGranularState {
                    non_granular_index: Arc::new(Mutex::new(0)),
                    tgt_non_granular_index,
                }
            }),
            is_lora,
        })))
    }

    fn get_id(&self) -> &str {
        self.xlora_model_id.as_deref().unwrap_or(&self.model_id)
    }

    fn get_kind(&self) -> ModelKind {
        ModelKind::QuantizedGGUF
    }
}

impl Pipeline for GgufPipeline {
    fn forward(
        &mut self,
        input_toks: &[&mut Sequence],
        is_prompt: bool,
    ) -> Result<Tensor, candle_core::Error> {
        let ModelInputs {
            input_ids,
            input_ids_full,
            seqlen_offsets,
            seqlen_offsets_full,
            seqlen_offsets_kernel,
            seqlen_offsets_kernel_full,
            context_lens,
        } = calculate_inputs(
            input_toks,
            is_prompt,
            self.is_xlora(),
            self.device(),
            self.no_kv_cache,
        )
        .unwrap();
        match self.model {
            Model::Llama(ref mut model) => model.forward(
                &input_ids,
                &seqlen_offsets,
                seqlen_offsets_kernel,
                context_lens,
            ),
            Model::Phi2(ref mut model) => model.forward(&input_ids, &seqlen_offsets, context_lens),
            Model::XLoraLlama(ref mut model) => model.forward(
                &input_ids,
                input_ids_full.as_ref().unwrap_or(&input_ids),
                &seqlen_offsets,
                seqlen_offsets_full.as_ref().unwrap_or(&seqlen_offsets),
                seqlen_offsets_kernel.clone(),
                seqlen_offsets_kernel_full.unwrap_or(seqlen_offsets_kernel),
                self.no_kv_cache,
                &self.non_granular_state,
                context_lens,
            ),
        }
    }
    fn device(&self) -> &Device {
        match self.model {
            Model::Llama(ref model) => &model.device,
            Model::Phi2(ref model) => &model.device,
            Model::XLoraLlama(ref model) => &model.device,
        }
    }
    fn num_hidden_layers(&self) -> usize {
        self.cache().lock().len()
    }
    fn cache(&self) -> &Cache {
        match self.model {
            Model::Llama(ref model) => &model.cache,
            Model::Phi2(ref model) => &model.cache,
            Model::XLoraLlama(ref model) => &model.cache,
        }
    }
    fn get_repeat_last_n(&self) -> usize {
        self.config.repeat_last_n
    }
    fn tokenizer(&self) -> Arc<Tokenizer> {
        self.tokenizer.clone()
    }
    fn eos_tok(&self) -> &[u32] {
        &self.eos_tok
    }
    fn name(&self) -> String {
        self.model_id.clone()
    }
    fn get_max_seq_len(&self) -> usize {
        match &self.model {
            Model::Llama(model) => model.max_seq_len,
            Model::Phi2(model) => model.max_seq_len,
            Model::XLoraLlama(model) => model.max_seq_len,
        }
    }
    fn is_xlora(&self) -> bool {
        match &self.model {
            Model::Llama(_) | Model::Phi2(_) => false,
            Model::XLoraLlama(_) => !self.is_lora,
        }
    }
    fn has_no_kv_cache(&self) -> bool {
        self.no_kv_cache
    }
    fn get_chat_template(&self) -> &ChatTemplate {
        &self.chat_template
    }
    fn get_non_granular_state(&self) -> &Option<NonGranularState> {
        &None
    }
    fn tok_trie(&self) -> &TokTrie {
        &self.tok_trie
    }
}
