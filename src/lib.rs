#![feature(iter_array_chunks)]
#![cfg_attr(
    all(feature = "prefetch", target_arch = "aarch64"),
    feature(stdarch_aarch64_prefetch)
)]
#![doc = include_str!("../README.md")]

use half::f16;

pub mod pylib;

pub mod sparse_dataset;

pub use sparse_dataset::SparseDataset;
pub use sparse_dataset::SparseDatasetMut;

pub mod inverted_index;

pub use inverted_index::InvertedIndex;

pub mod quantized_summary;

pub use quantized_summary::QuantizedSummary;

pub mod space_usage;

pub use space_usage::SpaceUsage;

pub mod distances;
pub mod topk_selectors;
pub mod utils;

use crate::pylib::PySeismicIndex;
use crate::pylib::PySeismicIndexLargeVocabulary;
use num_traits::{AsPrimitive, ToPrimitive, Zero};
use pyo3::prelude::PyModule;
use pyo3::{pymodule, PyResult, Python};


/// Marker for types used as components in a dataset
pub trait ComponentType: AsPrimitive<usize>  + SpaceUsage + Copy + Send + Sync + std::hash::Hash + Eq + Ord + std::convert::TryFrom<usize> {
}

impl ComponentType for u16 {}

impl ComponentType for u32 {}

/// Marker for types used as values in a dataset
pub trait DataType:
    SpaceUsage + Copy + AsPrimitive<f16> + ToPrimitive + Zero + Send + Sync
{
}

impl DataType for f64 {}

impl DataType for f32 {}

impl DataType for f16 {}

/// A Python module implemented in Rust. The name of this function must match the `lib.name`
/// setting in the `Cargo.toml`, otherwise Python will not be able to import the module.
#[pymodule]
fn seismic(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_class::<PySeismicIndex>()?;
    m.add_class::<PySeismicIndexLargeVocabulary>()?;
    Ok(())
}
