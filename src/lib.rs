mod audio;
mod tokenizers;
mod utils;

use audio::{get_mel_filteres, read_audio};
use ndarray_npy::NpzReader;
use rayon::prelude::*;
use std::fs::File;
use tokenizers::Tokenizer;
use tract_ndarray::{concatenate, s, Array, Array2, ArrayBase, Axis, Dim, IxDynImpl, OwnedRepr};
use tract_onnx::prelude::*;
use utils::{KVCache, Options};

pub struct Whisper {
    encoder: SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>,
    decoder: SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>,
    tokenizer: Tokenizer,
    pos_emb: ArrayBase<OwnedRepr<f32>, Dim<[usize; 3]>>,
    mel_filters: Array2<f32>,
    options: Options,
}

impl Whisper {
    pub fn new(
        encoder_path: &str,
        decoder_path: &str,
        tokenizer_path: &str,
        pos_emb_path: &str,
        mel_filters_path: &str,
    ) -> Whisper {
        let encoder = tract_onnx::onnx()
            .model_for_path(encoder_path)
            .unwrap()
            .into_optimized()
            .unwrap()
            .into_runnable()
            .unwrap();
        let decoder = tract_onnx::onnx()
            .model_for_path(decoder_path)
            .unwrap()
            .into_optimized()
            .unwrap()
            .into_runnable()
            .unwrap();
        let tokenizer = Tokenizer::new(tokenizer_path);
        let pos_emb = {
            let file = File::open(pos_emb_path).expect("Failed to open file");
            let mut npz = NpzReader::new(file).expect("Failed to read NPZ file");
            let pos_emb: Array2<f32> = npz.by_index(0).unwrap();
            pos_emb.insert_axis(Axis(0))
        };
        let mel_filters = get_mel_filteres(mel_filters_path);
        let options = Options::new();

        Whisper {
            encoder,
            decoder,
            tokenizer,
            pos_emb,
            mel_filters,
            options,
        }
    }

    fn get_audio_features(&self, mel: Array2<f32>) -> ArrayBase<OwnedRepr<f32>, Dim<IxDynImpl>> {
        let mel: Tensor = mel.insert_axis(Axis(0)).into();
        let inputs = tvec!(mel.into());
        let encoder_out = self.encoder.run(inputs).unwrap()[0]
            .to_array_view::<f32>()
            .unwrap()
            .to_owned();

        encoder_out
    }

    fn get_initial_tokens(&self, prompt: Vec<i32>, language: &str) -> Vec<i32> {
        let lang_token = self.tokenizer.lang2token.get(language).unwrap();
        let init_tokens: Vec<i32> = vec![50258, *lang_token as i32, 50359];

        if prompt.len() > 0 {
            let prev_prompt_len = self.options.n_ctx / 2 - 1;
            let prompt_tokens: Vec<i32>;

            if prompt.len() > prev_prompt_len {
                prompt_tokens = prompt[prompt.len() - prev_prompt_len..].to_vec();
            } else {
                prompt_tokens = prompt;
            }

            let tokens: Vec<i32> = vec![self.options.sot_prev as i32]
                .into_iter()
                .chain(prompt_tokens.into_iter())
                .collect();
            let tokens: Vec<i32> = tokens.into_iter().chain(init_tokens.into_iter()).collect();
            tokens
        } else {
            let tokens = vec![self.options.sot_prev as i32];
            let tokens: Vec<i32> = tokens.into_iter().chain(init_tokens.into_iter()).collect();
            tokens
        }
    }

    fn inference_logits(
        &self,
        tokens: Array<i32, Dim<[usize; 2]>>,
        audio_features: ArrayBase<OwnedRepr<f32>, Dim<IxDynImpl>>,
        kv_cache: KVCache,
        initial_token_length: usize,
    ) -> (ArrayBase<OwnedRepr<f32>, Dim<IxDynImpl>>, KVCache) {
        let offset = kv_cache.k1.shape()[1];
        let mut tokens = tokens;

        if tokens.shape()[1] > initial_token_length {
            tokens = tokens.slice(s![.., -1]).to_owned().insert_axis(Axis(0));
        }

        let pos_emb = self
            .pos_emb
            .slice(s![.., offset..offset + tokens.shape()[1], ..])
            .to_owned();

        let inputs = tvec!(
            tokens.into_tensor().into(),
            audio_features.into_tensor().into(),
            pos_emb.into_tensor().into(),
            kv_cache.k1.into(),
            kv_cache.v1.into(),
            kv_cache.k2.into(),
            kv_cache.v2.into(),
            kv_cache.k3.into(),
            kv_cache.v3.into(),
            kv_cache.k4.into(),
            kv_cache.v4.into(),
            kv_cache.k5.into(),
            kv_cache.v5.into(),
            kv_cache.k6.into(),
            kv_cache.v6.into(),
        );

        let out = self.decoder.run(inputs).unwrap();
        let logits = out[0].to_array_view::<f32>().unwrap().to_owned();
        let k1 = out[1].to_owned().into_tensor();
        let v1 = out[2].to_owned().into_tensor();
        let k2 = out[3].to_owned().into_tensor();
        let v2 = out[4].to_owned().into_tensor();
        let k3 = out[5].to_owned().into_tensor();
        let v3 = out[6].to_owned().into_tensor();
        let k4 = out[7].to_owned().into_tensor();
        let v4 = out[8].to_owned().into_tensor();
        let k5 = out[9].to_owned().into_tensor();
        let v5 = out[10].to_owned().into_tensor();
        let k6 = out[11].to_owned().into_tensor();
        let v6 = out[12].to_owned().into_tensor();

        let new_kv_cache = KVCache {
            k1,
            k2,
            k3,
            k4,
            k5,
            k6,
            v1,
            v2,
            v3,
            v4,
            v5,
            v6,
        };

        (logits, new_kv_cache)
    }

    fn inference(
        &self,
        audio_features: ArrayBase<OwnedRepr<f32>, Dim<IxDynImpl>>,
        prompt: Vec<i32>,
        language: &str,
    ) -> Vec<i32> {
        let initial_tokens = self.get_initial_tokens(prompt, language);
        let initial_token_length = initial_tokens.len();

        let mut tokens: Array<i32, Dim<[usize; 2]>> =
            Array::from_vec(initial_tokens).insert_axis(Axis(0));
        let mut kv_cache = KVCache::default();

        for _ in 0..224 {
            let logits: ArrayBase<OwnedRepr<f32>, Dim<IxDynImpl>>;
            (logits, kv_cache) = self.inference_logits(
                tokens.clone(),
                audio_features.clone(),
                kv_cache.clone(),
                initial_token_length,
            );
            let next_word = logits
                .slice(s![.., -1, ..])
                .iter()
                .enumerate()
                .max_by(|(_, u), (_, v)| u.total_cmp(v))
                .map(|(i, _)| i as usize)
                .unwrap();

            if next_word == self.options.eot_token || tokens.shape()[1] > self.options.n_ctx {
                break;
            }

            let next_word_array = Array::from_elem((1, 1), next_word as i32);
            tokens = concatenate!(Axis(1), tokens, next_word_array);
        }
        tokens = tokens.slice(s![.., initial_token_length..]).to_owned();
        return tokens.into_raw_vec();
    }

    fn run(&self, mel: Array2<f32>, language: &str) -> String {
        let num_frames = mel.shape()[1];
        let mut seek = 0;
        let mut segments = vec![];

        while seek < num_frames {
            let segment: Array2<f32>;

            if seek + audio::N_FRAMES < mel.shape()[1] {
                segment = mel.slice(s![.., seek..seek + audio::N_FRAMES]).to_owned();
            } else {
                segment = mel.slice(s![.., seek..]).to_owned();
            }

            segments.push(audio::pad_or_trim(segment, audio::N_FRAMES));
            seek += audio::N_FRAMES;
        }
        let audio_features: Vec<ArrayBase<OwnedRepr<f32>, Dim<IxDynImpl>>> = segments
            .par_iter()
            .map(|segment| self.get_audio_features(segment.clone()))
            .collect();
        let mut result: Vec<i32> = vec![];
        for audio_feature in audio_features {
            let tokens = self.inference(audio_feature, result.clone(), language);
            result.extend(tokens.clone());
        }

        self.tokenizer.decode(
            result
                .iter()
                .map(|v| *v as u32)
                .filter(|item| item < &50257)
                .collect(),
        )
    }

    pub fn recognize_from_audio(&self, audio_path: &str, language: &str) -> String {
        let audio_data = read_audio(audio_path).unwrap();
        let mel = audio::log_mel_spectrogram(audio_data, self.mel_filters.clone());
        self.run(mel, language)
    }
}
