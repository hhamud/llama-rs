use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{
    util::{self, FindAllModelFilesError},
    Hyperparameters, KnownModel, ModelParameters, TokenId, Vocabulary,
};
pub use ggml::ContainerType;
use ggml::{
    format::{LoadError as FormatLoadError, PartialHyperparameters, TensorLoadInfo},
    Context,
};
use memmap2::Mmap;
use thiserror::Error;

/// How the tensors are stored in GGML LLM models.
#[derive(Debug, PartialEq, Clone, Copy, Eq, Default)]
pub enum FileType {
    /// All tensors are stored as f32.
    F32,
    #[default]
    /// All tensors are mostly stored as `f16`, except for the 1D tensors (32-bit).
    MostlyF16,
    /// All tensors are mostly stored as `Q4_0`, except for the 1D tensors (32-bit).
    MostlyQ4_0,
    /// All tensors are mostly stored as `Q4_1`, except for the 1D tensors (32-bit)
    MostlyQ4_1,
    /// All tensors are mostly stored as `Q4_1`, except for the 1D tensors (32-bit)
    /// and the `tok_embeddings.weight` (f16) and `output.weight` tensors (f16).
    MostlyQ4_1SomeF16,
    /// All tensors are mostly stored as `Q4_2`, except for the 1D tensors (32-bit).
    MostlyQ4_2,
    /// All tensors are mostly stored as `Q8_0`, except for the 1D tensors (32-bit).
    MostlyQ8_0,
    /// All tensors are mostly stored as `Q5_0`, except for the 1D tensors (32-bit).
    MostlyQ5_0,
    /// All tensors are mostly stored as `Q5_1`, except for the 1D tensors (32-bit).
    MostlyQ5_1,
}
impl From<FileType> for i32 {
    fn from(value: FileType) -> Self {
        match value {
            FileType::F32 => 0,
            FileType::MostlyF16 => 1,
            FileType::MostlyQ4_0 => 2,
            FileType::MostlyQ4_1 => 3,
            FileType::MostlyQ4_1SomeF16 => 4,
            FileType::MostlyQ4_2 => 5,
            FileType::MostlyQ8_0 => 7,
            FileType::MostlyQ5_0 => 8,
            FileType::MostlyQ5_1 => 9,
        }
    }
}
impl TryFrom<i32> for FileType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FileType::F32),
            1 => Ok(FileType::MostlyF16),
            2 => Ok(FileType::MostlyQ4_0),
            3 => Ok(FileType::MostlyQ4_1),
            4 => Ok(FileType::MostlyQ4_1SomeF16),
            5 => Ok(FileType::MostlyQ4_2),
            7 => Ok(FileType::MostlyQ8_0),
            8 => Ok(FileType::MostlyQ5_0),
            9 => Ok(FileType::MostlyQ5_1),
            _ => Err(()),
        }
    }
}
impl Display for FileType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FileType::F32 => write!(f, "f32"),
            FileType::MostlyF16 => write!(f, "f16"),
            FileType::MostlyQ4_0 => write!(f, "q4_0"),
            FileType::MostlyQ4_1 => write!(f, "q4_1"),
            FileType::MostlyQ4_1SomeF16 => write!(f, "q4_1_with_f16"),
            FileType::MostlyQ4_2 => write!(f, "q4_2"),
            FileType::MostlyQ8_0 => write!(f, "q8_0"),
            FileType::MostlyQ5_0 => write!(f, "q5_0"),
            FileType::MostlyQ5_1 => write!(f, "q5_1"),
        }
    }
}

/// Each variant represents a step within the process of loading the model.
/// These can be used to report progress to the user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoadProgress {
    /// The hyperparameters have been loaded from the model.
    HyperparametersLoaded,
    /// The context has been created.
    ContextSize {
        /// The size of the context.
        bytes: usize,
    },
    /// A tensor from the current part has been loaded.
    TensorLoaded {
        /// The current tensor (0-indexed).
        current_tensor: usize,
        /// The number of total tensors.
        tensor_count: usize,
    },
    /// A model part has finished fully loading.
    Loaded {
        /// The number of bytes in the part.
        file_size: u64,
        /// The number of tensors in the part.
        tensor_count: usize,
    },
}

#[derive(Error, Debug)]
/// Errors encountered during the loading process.
pub enum LoadError {
    #[error("could not open file {path:?}")]
    /// A file failed to open.
    OpenFileFailed {
        /// The original error.
        source: std::io::Error,
        /// The path that failed.
        path: PathBuf,
    },
    #[error("no parent path for {path:?}")]
    /// There is no parent path for a given path.
    NoParentPath {
        /// The path without a parent.
        path: PathBuf,
    },
    #[error("unable to read exactly {bytes} bytes")]
    /// Reading exactly `bytes` from a file failed.
    ReadExactFailed {
        /// The original error.
        source: std::io::Error,
        /// The number of bytes that were attempted to be read.
        bytes: usize,
    },
    #[error("non-specific I/O error")]
    /// A non-specific IO error.
    Io(#[from] std::io::Error),
    #[error("could not convert bytes to a UTF-8 string")]
    /// One of the strings encountered was not valid UTF-8.
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("invalid integer conversion")]
    /// One of the integers encountered could not be converted to a more appropriate type.
    InvalidIntegerConversion(#[from] std::num::TryFromIntError),
    #[error("unsupported f16_: {0}")]
    /// The `f16_` hyperparameter had an invalid value.
    UnsupportedFileType(i32),
    #[error("invalid magic number {magic:#x} for {path:?}")]
    /// An invalid magic number was encountered during the loading process.
    InvalidMagic {
        /// The path that failed.
        path: PathBuf,
        /// The magic number that was encountered.
        magic: u32,
    },
    #[error("invalid file format version {version}")]
    /// The version of the format is not supported by this version of `llm`.
    InvalidFormatVersion {
        /// The format that was encountered.
        container_type: ContainerType,
        /// The version that was encountered.
        version: u32,
    },
    #[error("invalid value {ftype} for `f16` in hyperparameters")]
    /// The `f16` hyperparameter had an invalid value.
    HyperparametersF16Invalid {
        /// The format type that was encountered.
        ftype: i32,
    },
    #[error("unknown tensor `{tensor_name}` in {path:?}")]
    /// The tensor `tensor_name` was encountered during the loading of `path`, but was not seen during
    /// the model prelude.
    UnknownTensor {
        /// The name of the tensor.
        tensor_name: String,
        /// The path that failed.
        path: PathBuf,
    },
    #[error("the tensor `{tensor_name}` has the wrong size in {path:?}")]
    /// The tensor `tensor_name` did not match its expected size.
    TensorWrongSize {
        /// The name of the tensor.
        tensor_name: String,
        /// The path that failed.
        path: PathBuf,
    },
    /// The tensor `tensor_name` did not have the expected format type.
    #[error("invalid ftype {ftype} for tensor `{tensor_name}` in {path:?}")]
    UnsupportedElementType {
        /// The name of the tensor.
        tensor_name: String,
        /// The format type that was encountered.
        ftype: u32,
        /// The path that failed.
        path: PathBuf,
    },
    /// An invariant was broken.
    ///
    /// This error is not relevant unless `loader2` is being used.
    #[error("invariant broken: {invariant} in {path:?}")]
    InvariantBroken {
        /// The path that failed.
        path: Option<PathBuf>,
        /// The invariant that was broken.
        invariant: String,
    },
    /// The model could not be created.
    ///
    /// This implies that there were no tensors in the model to be loaded.
    ///
    /// This error is not relevant unless `loader2` is being used.
    #[error("could not create model from {path:?}")]
    ModelNotCreated {
        /// The path that failed.
        path: PathBuf,
    },
    /// Multiple parts of the model were found.
    ///
    /// Multi-part models are not supported. Please convert the model to a single part.
    #[error("multipart models are not supported")]
    MultipartNotSupported {
        /// The paths that were found.
        paths: Vec<PathBuf>,
    },
}
impl From<FindAllModelFilesError> for LoadError {
    fn from(value: FindAllModelFilesError) -> Self {
        match value {
            FindAllModelFilesError::NoParentPath { path } => LoadError::NoParentPath { path },
            FindAllModelFilesError::IO(err) => LoadError::Io(err),
        }
    }
}

impl LoadError {
    #[doc(hidden)]
    pub fn from_format_error(value: FormatLoadError<LoadError>, path: PathBuf) -> Self {
        match value {
            FormatLoadError::InvalidMagic(magic) => LoadError::InvalidMagic { path, magic },
            FormatLoadError::InvalidFormatVersion(container_type, version) => {
                LoadError::InvalidFormatVersion {
                    container_type,
                    version,
                }
            }
            FormatLoadError::Io(err) => LoadError::Io(err),
            FormatLoadError::InvalidUtf8(err) => LoadError::InvalidUtf8(err),
            FormatLoadError::InvalidIntegerConversion(err) => {
                LoadError::InvalidIntegerConversion(err)
            }
            FormatLoadError::ImplementationError(err) => err,
            FormatLoadError::UnsupportedElementType { tensor_name, ftype } => {
                LoadError::UnsupportedElementType {
                    path,
                    tensor_name,
                    ftype,
                }
            }
            FormatLoadError::InvariantBroken(invariant) => LoadError::InvariantBroken {
                path: Some(path),
                invariant,
            },
        }
    }
}

/// Used by models to fetch tensors from a loader.
pub trait TensorLoader<E: std::error::Error> {
    /// Gets a tensor from the loader.
    fn load(&mut self, name: &str) -> Result<ggml::Tensor, E>;
    /// Loads a tensor from the loader.
    fn load_manual(&mut self, name: &str, ne: &[usize]) -> Result<ggml::Tensor, E>;
    /// Finish loading the model, and extract all of the state from the loader.
    fn finish(self) -> (Context, HashMap<String, ggml::Tensor>, Option<Mmap>);
}

/// Load an arbitrary GGML model.
///
/// Note that the model must be a single-part model, and the model in `path`
/// *must* match the architecture of `M`.
///
/// # Panics
///
/// - If the model does not match the architecture of `M`. This is not checked
///   before execution, so this function will panic if the model does not match
///   the architecture.
///
///   This is a limitation of the GGML format, which does not
///   store any information about the architecture.
pub fn load<M: KnownModel>(
    path: &Path,
    params: ModelParameters,
    load_progress_callback: impl FnMut(LoadProgress),
) -> Result<M, LoadError> {
    let paths = util::find_all_model_files(path)?;
    if paths.len() != 1 {
        return Err(LoadError::MultipartNotSupported { paths });
    }

    let file = File::open(path).map_err(|e| LoadError::OpenFileFailed {
        source: e,
        path: path.to_owned(),
    })?;
    let mut reader = BufReader::new(&file);

    let mut loader = Loader::new(load_progress_callback);

    ggml::format::load(&mut reader, &mut loader)
        .map_err(|err| LoadError::from_format_error(err, path.to_owned()))?;

    let Loader {
        hyperparameters,
        vocabulary,
        tensors,
        mut load_progress_callback,
        container_type,
        ..
    } = loader;

    let use_mmap = params.prefer_mmap && container_type.support_mmap();

    let ctx_size = tensors
        .values()
        .map(|ti| {
            ggml::Tensor::C_TYPE_SIZE
                + ggml::OBJECT_SIZE
                + if use_mmap { 0 } else { ti.calc_size() }
        })
        .sum::<usize>();
    (load_progress_callback)(LoadProgress::ContextSize { bytes: ctx_size });
    let context = Context::init(ctx_size, !use_mmap);

    let (mmap, file_size) = {
        let file = File::open(path)?;
        let mmap = if use_mmap {
            Some(unsafe { Mmap::map(&file)? })
        } else {
            None
        };
        (mmap, file.metadata()?.len())
    };

    struct MmapCompatibleLoader<'a> {
        path: PathBuf,
        file: File,
        tensors: HashMap<String, TensorLoadInfo>,
        context: Context,
        mmap: Option<Mmap>,
        load_progress_callback: &'a mut dyn FnMut(LoadProgress),
        loaded_tensors: HashMap<String, ggml::Tensor>,
    }
    impl TensorLoader<LoadError> for MmapCompatibleLoader<'_> {
        fn load(&mut self, name: &str) -> Result<ggml::Tensor, LoadError> {
            let tensor_dims = self
                .tensors
                .get(name)
                .map(|tensor| tensor.dims().to_vec())
                .ok_or(LoadError::UnknownTensor {
                    tensor_name: String::from(name),
                    path: Default::default(),
                })?;
            self.load_manual(name, &tensor_dims)
        }

        fn load_manual(&mut self, name: &str, ne: &[usize]) -> Result<ggml::Tensor, LoadError> {
            let info = self
                .tensors
                .get(name)
                .ok_or_else(|| LoadError::UnknownTensor {
                    path: self.path.clone(),
                    tensor_name: name.to_owned(),
                })?;

            let dims = ne.len();
            if dims != info.n_dims {
                return Err(LoadError::InvariantBroken {
                    path: Some(self.path.clone()),
                    invariant: format!(
                        "the tensor {name} should have {} dimensions, not {dims}",
                        info.n_dims
                    ),
                });
            }

            let ctx = &self.context;
            let mut tensor = match dims {
                1 => ctx.new_tensor_1d(info.element_type, ne[0]),
                2 => ctx.new_tensor_2d(info.element_type, ne[0], ne[1]),
                3 => ctx.new_tensor_3d(info.element_type, ne[0], ne[1], ne[2]),
                _ => {
                    return Err(LoadError::InvariantBroken {
                        path: Some(self.path.clone()),
                        invariant: format!(
                            "the tensor {name} had an unsupported dimension count: {ne:?}"
                        ),
                    })
                }
            };

            match self.mmap.as_ref() {
                Some(mmap) => unsafe {
                    let ptr = mmap.as_ptr().offset(info.start_offset as isize);
                    tensor.set_data(ptr as *mut std::ffi::c_void);
                },
                None => {
                    let buf: &mut [u8] = unsafe {
                        std::slice::from_raw_parts_mut(tensor.data() as *mut u8, tensor.nbytes())
                    };
                    self.file.seek(SeekFrom::Start(info.start_offset))?;
                    self.file.read_exact(buf)?;
                }
            }

            self.loaded_tensors.insert(name.to_owned(), tensor.share());
            (self.load_progress_callback)(LoadProgress::TensorLoaded {
                current_tensor: self.loaded_tensors.len(),
                tensor_count: self.tensors.len(),
            });

            Ok(tensor)
        }

        fn finish(self) -> (Context, HashMap<String, ggml::Tensor>, Option<Mmap>) {
            (self.context, self.loaded_tensors, self.mmap)
        }
    }

    let tensors_len = tensors.len();
    let tl = MmapCompatibleLoader {
        path: path.to_owned(),
        file,
        tensors,
        context,
        mmap,
        load_progress_callback: &mut load_progress_callback,
        loaded_tensors: Default::default(),
    };

    let model = KnownModel::new(hyperparameters, params, vocabulary, tl)?;

    (load_progress_callback)(LoadProgress::Loaded {
        file_size,
        tensor_count: tensors_len,
    });

    Ok(model)
}

/// A GGML format loader for LLMs.
pub struct Loader<Hp: Hyperparameters, F: FnMut(LoadProgress)> {
    // Input
    load_progress_callback: F,

    // Output
    /// The container type of the model.
    pub container_type: ContainerType,
    /// The hyperparameters of the model.
    pub hyperparameters: Hp,
    /// The vocabulary of the model.
    pub vocabulary: Vocabulary,
    /// The tensors of the model.
    pub tensors: HashMap<String, TensorLoadInfo>,
}
impl<Hp: Hyperparameters, F: FnMut(LoadProgress)> Loader<Hp, F> {
    /// Creates a new loader.
    pub fn new(load_progress_callback: F) -> Self {
        Self {
            load_progress_callback,

            container_type: ContainerType::Ggjt,
            hyperparameters: Hp::default(),
            vocabulary: Vocabulary::default(),
            tensors: HashMap::default(),
        }
    }
}
impl<Hp: Hyperparameters, F: FnMut(LoadProgress)> ggml::format::LoadHandler<LoadError>
    for Loader<Hp, F>
{
    fn container_type(&mut self, container_type: ContainerType) -> Result<(), LoadError> {
        self.container_type = container_type;
        Ok(())
    }

    fn vocabulary_token(&mut self, i: usize, token: Vec<u8>, score: f32) -> Result<(), LoadError> {
        let id = match TokenId::try_from(i) {
            Ok(id) => id,
            Err(err) => return Err(LoadError::InvalidIntegerConversion(err)),
        };
        self.vocabulary.push_token(id, token, score);

        Ok(())
    }

    fn read_hyperparameters(
        &mut self,
        reader: &mut dyn BufRead,
    ) -> Result<PartialHyperparameters, LoadError> {
        // NOTE: Field order matters! Data is laid out in the file exactly in this order.
        let hyperparameters = Hp::read(reader)?;
        let partial = PartialHyperparameters {
            n_vocab: hyperparameters.n_vocabulary(),
        };
        self.hyperparameters = hyperparameters;
        (self.load_progress_callback)(LoadProgress::HyperparametersLoaded);

        Ok(partial)
    }

    fn tensor_buffer(&mut self, info: TensorLoadInfo) -> Result<(), LoadError> {
        self.tensors.insert(info.name.clone(), info);
        Ok(())
    }
}

/// A implementation for `load_progress_callback` that outputs to `stdout`.
pub fn load_progress_callback_stdout(progress: LoadProgress) {
    match progress {
        LoadProgress::HyperparametersLoaded => println!("Loaded hyperparameters"),
        LoadProgress::ContextSize { bytes } => println!(
            "ggml ctx size = {:.2} MB\n",
            bytes as f64 / (1024.0 * 1024.0)
        ),
        LoadProgress::TensorLoaded {
            current_tensor,
            tensor_count,
            ..
        } => {
            let current_tensor = current_tensor + 1;
            if current_tensor % 8 == 0 {
                println!("Loaded tensor {current_tensor}/{tensor_count}");
            }
        }
        LoadProgress::Loaded {
            file_size: byte_size,
            tensor_count,
        } => {
            println!("Loading of model complete");
            println!(
                "Model size = {:.2} MB / num tensors = {}",
                byte_size as f64 / 1024.0 / 1024.0,
                tensor_count
            );
        }
    };
}
