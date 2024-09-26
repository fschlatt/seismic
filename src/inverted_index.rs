use crate::distances::{dot_product_dense_sparse, dot_product_with_merge};
use crate::sparse_dataset::SparseDatasetMut;
use crate::topk_selectors::{HeapFaiss, OnlineTopKSelector};
use crate::utils::{do_random_kmeans_on_docids, prefetch_read_NTA};
use crate::{QuantizedSummary, SpaceUsage, SparseDataset};
use crate::{ComponentType, DataType};

use indicatif::ParallelProgressIterator;

use itertools::Itertools;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Default, PartialEq, Debug, Clone, Serialize, Deserialize)]
pub struct InvertedIndex<C, T>
where
    C: ComponentType,  T: DataType,
{
    forward_index: SparseDataset<C, T>,
    posting_lists: Box<[PostingList]>,
    config: Configuration,
}

impl<C, T> SpaceUsage for InvertedIndex<C, T>
where
C: ComponentType,  T: DataType,
{
    fn space_usage_byte(&self) -> usize {
        let forward = self.forward_index.space_usage_byte();

        let postings: usize = self
            .posting_lists
            .iter()
            .map(|list| list.space_usage_byte())
            .sum();

        forward + postings
    }
}

/// This struct should contain every configuraion parameter for building the index
/// that doesn't need to be "managed" at query time.
/// Examples are the pruning strategy and the clustering strategy.
/// These can be chosen with a if at building time but there is no need to
/// make any choice at query time.
///
/// Howerver, there are parameters that influence choices at query time.
/// To avoid branches or dynamic dispatching, this kind of parametrizaton are
/// selected with generic types.
/// An example is the quantization strategy. Based on the chosen
/// quantization strategy, we need to chose the right function to call while
/// computing the distance between vectors.
///
/// HERE WE COULD JUST HAVE A GENERIC IN THE SEARCH FUNCTION.
/// Have a non specilized search with a match and a call to the correct function!

#[derive(Default, PartialEq, Debug, Clone, Serialize, Deserialize)]
pub struct Configuration {
    pruning: PruningStrategy,
    blocking: BlockingStrategy,
    summarization: SummarizationStrategy,
}

impl Configuration {
    pub fn pruning_strategy(mut self, pruning: PruningStrategy) -> Self {
        self.pruning = pruning;

        self
    }

    pub fn blocking_strategy(mut self, blocking: BlockingStrategy) -> Self {
        self.blocking = blocking;

        self
    }

    pub fn summarization_strategy(mut self, summarization: SummarizationStrategy) -> Self {
        self.summarization = summarization;

        self
    }
}

const THRESHOLD_BINARY_SEARCH: usize = 10;

impl<C, T> InvertedIndex<C, T>
where
C: ComponentType,  T: PartialOrd + DataType,
{
    /// Help function to print the space usage of the index.
    pub fn print_space_usage_byte(&self) -> usize {
        println!("Space Usage:");
        let forward = self.forward_index.space_usage_byte();
        println!("\tForward Index: {:} Bytes", forward);
        let postings: usize = self
            .posting_lists
            .iter()
            .map(|list| list.space_usage_byte())
            .sum();

        println!("\tPosting Lists: {:} Bytes", postings);
        println!("\tTotal: {:} Bytes", forward + postings);

        forward + postings
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    #[inline]
    pub fn search(
        &self,
        query_components: &[C], // FIXME NOW WE ARE USING U16, WE SHOULD USE C
        query_values: &[f32],
        k: usize,
        query_cut: usize,
        heap_factor: f32,
    ) -> Vec<(f32, usize)> {
        let mut query = vec![0.0; self.dim()];

        for (&i, &v) in query_components.iter().zip(query_values) {
            query[i.as_()] = v;
        }
        let mut heap = HeapFaiss::new(k);
        let mut visited = HashSet::with_capacity(query_cut * 5000); // 5000 should be n_postings

        // Sort query terms by score and evaluate the posting list only for the top ones
        for (&component_id, &_value) in query_components
            .iter()
            .zip(query_values)
            .sorted_unstable_by(|a, b| b.1.partial_cmp(a.1).unwrap())
            .take(query_cut)
        {
            self.posting_lists[component_id.as_()].search(
                &query,
                query_components,
                query_values,
                k,
                heap_factor,
                &mut heap,
                &mut visited,
                &self.forward_index,
            );
        }

        heap.topk()
            .iter()
            .map(|&(dot, offset)| (dot.abs(), self.forward_index.offset_to_id(offset)))
            .collect()
    }

    /// `n_postings`: minimum number of postings to select for each component
    pub fn build(dataset: SparseDataset<C, T>, config: Configuration) -> Self
     where <C as TryFrom<usize>>::Error: std::fmt::Debug {
        // Distribute pairs (score, doc_id) to corresponding components.
        // We use pairs because later each posting list will be sorted by score
        // by the pruning strategy.

        print!("\tDistributing postings ");
        let time = Instant::now();
        let mut inverted_pairs = Vec::with_capacity(dataset.dim());
        for _ in 0..dataset.dim() {
            inverted_pairs.push(Vec::new());
        }

        for (doc_id, (components, values)) in dataset.iter().enumerate() {
            for (&c, &score) in components.iter().zip(values) {
                inverted_pairs[c.as_()].push((score, doc_id));
            }
        }

        let elapsed = time.elapsed();
        println!("{} secs", elapsed.as_secs());

        // Apply the selected pruning strategy

        print!("\tPruning postings ");
        let time = Instant::now();

        match config.pruning {
            PruningStrategy::FixedSize { n_postings } => {
                Self::fixed_pruning(&mut inverted_pairs, n_postings)
            }

            PruningStrategy::GlobalThreshold {
                n_postings,
                max_fraction,
            } => {
                Self::global_threshold_pruning(&mut inverted_pairs, n_postings);
                Self::fixed_pruning(
                    &mut inverted_pairs,
                    (n_postings as f32 * max_fraction) as usize,
                ) // cuts too long lists
            }
        }

        let elapsed = time.elapsed();
        println!("{} secs", elapsed.as_secs());

        print!("\tBuilding summaries ");
        let time = Instant::now();

        println!("\tNumber of posting lists: {}", inverted_pairs.len());
        // Build summaries and blocks for each posting list
        let posting_lists: Vec<_> = inverted_pairs
            .par_iter()
            .progress_count(inverted_pairs.len() as u64)
            .enumerate()
            .map(|(_component_id, posting_list)| {
                //println!("\tDealing with component {_component_id}");
                PostingList::build(&dataset, posting_list, &config)
            })
            .collect();

        let elapsed = time.elapsed();
        println!("{} secs", elapsed.as_secs());

        Self {
            forward_index: dataset,
            posting_lists: posting_lists.into_boxed_slice(),
            config,
        }
    }

    // Implementation of the pruning strategy that selects the top-`n_postings` from each posting list
    fn fixed_pruning(inverted_pairs: &mut Vec<Vec<(T, usize)>>, n_postings: usize) {
        inverted_pairs.par_iter_mut().for_each(|posting_list| {
            posting_list.sort_unstable_by(|a, b| b.0.partial_cmp(&a.0).unwrap());

            posting_list.truncate(n_postings);

            posting_list.shrink_to_fit();
        })
    }

    // Implementation of the pruning strategy that selects a threshold such that survives on average `n_postings` for each posting list
    fn global_threshold_pruning(inverted_pairs: &mut [Vec<(T, usize)>], n_postings: usize) where <C as TryFrom<usize>>::Error: std::fmt::Debug {
        let tot_postings = inverted_pairs.len() * n_postings; // overall number of postings to select

        // for every posting we create the tuple <score, docid, id_posting_list>
        let mut postings = Vec::<(T, usize, C)>::new();
        for (id, posting_list) in inverted_pairs.iter_mut().enumerate() {
            for (score, docid) in posting_list.iter() {
                postings.push((*score, *docid, id.try_into().unwrap()));
            }
            posting_list.clear();
        }

        let tot_postings = tot_postings.min(postings.len() - 1);

        postings.select_nth_unstable_by(tot_postings, |a, b| b.0.partial_cmp(&a.0).unwrap());

        for (score, docid, id_posting) in postings.into_iter().take(tot_postings) {
            inverted_pairs[id_posting.as_()].push((score, docid));
        }
    }

    /// Returns the id of the largest component, i.e., the dimensionality of the vectors in the dataset.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.forward_index.dim()
    }

    /// Returns the number of non-zero components in the dataset.
    #[must_use]
    pub fn nnz(&self) -> usize {
        self.forward_index.nnz()
    }

    /// Returns the number of vectors in the dataset
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward_index.len()
    }

    /// Checks if the dataset is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward_index.len() == 0
    }
}

// Instead of string doc_ids we store their offsets in the forward_index and the lengths of the vectors
// This allows us to save the random acceses that would be needed to access exactly these values from the
// forward index. The values of each doc are packed into a single u64 in `packed_postings`. We use 48 bits for the offset and 16 bits for the lenght. This choice limits the size of the dataset to be 1<<48-1.
// We use the forward index to convert the offsets of the top-k back to the id of the corresponding documents.
#[derive(Default, PartialEq, Debug, Clone, Serialize, Deserialize)]
struct PostingList {
    // postings: Box<[usize]>,
    packed_postings: Box<[u64]>,
    block_offsets: Box<[usize]>,
    // summaries: SparseDataset<f16>,
    summaries: QuantizedSummary,
}

impl SpaceUsage for PostingList {
    fn space_usage_byte(&self) -> usize {
        self.packed_postings.space_usage_byte()
            + self.block_offsets.space_usage_byte()
            + self.summaries.space_usage_byte()
    }
}

impl PostingList {
    #[inline]
    fn pack_offset_len(offset: usize, len: usize) -> u64 {
        ((offset as u64) << 16) | (len as u64)
    }

    #[inline]
    fn unpack_offset_len(pack: u64) -> (usize, usize) {
        ((pack >> 16) as usize, (pack & (u16::MAX as u64)) as usize)
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn search<C, T>(
        &self,
        query: &[f32],
        query_components: &[C],
        query_values: &[f32],
        k: usize,
        heap_factor: f32,
        heap: &mut HeapFaiss,
        visited: &mut HashSet<usize>,
        forward_index: &SparseDataset<C, T>,
    ) where
        C: ComponentType,
        T: DataType,
    {
        let mut blocks_to_evaluate: Vec<&[u64]> = Vec::new();
        let dots = self
            .summaries
            .matmul_with_query(query_components, query_values);
        //for (block_id, (c_summary, v_summary)) in self.summaries.iter().enumerate() {
        //let dot = dot_product_dense_sparse(query, c_summary, v_summary);
        for (block_id, &dot) in dots.iter().enumerate() {
            if heap.len() == k && dot < -heap_factor * heap.top() {
                continue;
            }

            let packed_posting_block = &self.packed_postings
                [self.block_offsets[block_id]..self.block_offsets[block_id + 1]];

            if blocks_to_evaluate.len() == 1 {
                for cur_packed_posting in blocks_to_evaluate.iter() {
                    self.evaluate_posting_block(
                        query,
                        query_components,
                        query_values,
                        cur_packed_posting,
                        heap,
                        visited,
                        forward_index,
                    );
                }
                blocks_to_evaluate.clear();
            }

            for i in (0..packed_posting_block.len()).step_by(8) {
                prefetch_read_NTA(packed_posting_block, i);
            }

            blocks_to_evaluate.push(packed_posting_block);
        }

        for cur_packed_posting in blocks_to_evaluate.iter() {
            self.evaluate_posting_block(
                query,
                query_components,
                query_values,
                cur_packed_posting,
                heap,
                visited,
                forward_index,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn evaluate_posting_block<C, T>(
        &self,
        query: &[f32],
        query_term_ids: &[C],
        query_values: &[f32],
        packed_posting_block: &[u64],
        heap: &mut HeapFaiss,
        visited: &mut HashSet<usize>,
        forward_index: &SparseDataset<C, T>,
    ) where
    C: ComponentType,  T: DataType,
    {
        let (mut prev_offset, mut prev_len) = Self::unpack_offset_len(packed_posting_block[0]);

        for &pack in packed_posting_block.iter().skip(1) {
            let (offset, len) = Self::unpack_offset_len(pack);
            forward_index.prefetch_vec_with_offset(offset, len);

            if !visited.contains(&prev_offset) {
                let (v_components, v_values) = forward_index.get_with_offset(prev_offset, prev_len);
                //let distance = dot_product_dense_sparse(query, v_components, v_values);
                let distance = if query_term_ids.len() < THRESHOLD_BINARY_SEARCH {
                    //dot_product_with_binary_search(
                    dot_product_with_merge(query_term_ids, query_values, v_components, v_values)
                } else {
                    dot_product_dense_sparse(query, v_components, v_values)
                };

                visited.insert(prev_offset);
                heap.push_with_id(-1.0 * distance, prev_offset);
            }

            prev_offset = offset;
            prev_len = len;
        }

        if visited.contains(&prev_offset) {
            return;
        }

        let (v_components, v_values) = forward_index.get_with_offset(prev_offset, prev_len);
        let distance = if query_term_ids.len() < THRESHOLD_BINARY_SEARCH {
            //dot_product_with_binary_search(
            dot_product_with_merge(query_term_ids, query_values, v_components, v_values)
        } else {
            dot_product_dense_sparse(query, v_components, v_values)
        };

        visited.insert(prev_offset);
        heap.push_with_id(-1.0 * distance, prev_offset);
    }

    /// Gets a posting list already pruned and represents it by using a blocking
    /// strategy to partition postings into block and a summarization strategy to
    /// represents the summary of each block.
    pub fn build<C, T>(
        dataset: &SparseDataset<C, T>,
        postings: &[(T, usize)],
        config: &Configuration,
    ) -> Self
    where
    C: ComponentType,  T: PartialOrd + DataType,
    {
        let mut posting_list: Vec<_> = postings.iter().map(|(_, docid)| *docid).collect();

        let block_offsets = match config.blocking {
            BlockingStrategy::FixedSize { block_size } => {
                Self::fixed_size_blocking(&posting_list, block_size)
            }

            BlockingStrategy::RandomKmeans {
                centroid_fraction,
                truncated_kmeans_training,
                truncation_size,
                min_cluster_size,
            } => Self::blocking_with_random_kmeans(
                &mut posting_list,
                centroid_fraction,
                truncated_kmeans_training,
                truncation_size,
                min_cluster_size,
                dataset,
            ),
        };

        let mut summaries = SparseDatasetMut::<C, T>::new();

        for block_range in block_offsets.windows(2) {
            let (components, values) = match config.summarization {
                SummarizationStrategy::FixedSize { n_components } => Self::fixed_size_summary(
                    dataset,
                    &posting_list[block_range[0]..block_range[1]],
                    n_components,
                ),

                SummarizationStrategy::EnergyPerserving {
                    summary_energy: fraction,
                } => Self::energy_preserving_summary(
                    dataset,
                    &posting_list[block_range[0]..block_range[1]],
                    fraction,
                ),
            };

            summaries.push(&components, &values);
        }

        let packed_postings: Vec<_> = posting_list
            .iter()
            .map(|doc_id| {
                Self::pack_offset_len(dataset.vector_offset(*doc_id), dataset.vector_len(*doc_id))
            })
            .collect();

        Self {
            packed_postings: packed_postings.into_boxed_slice(),
            block_offsets: block_offsets.into_boxed_slice(),
            summaries: QuantizedSummary::new(
                SparseDataset::<C, T>::from(summaries).quantize_f16(),
                dataset.dim(),
            ),
        }
    }

    // ** Blocking strategies **

    fn fixed_size_blocking(posting_list: &[usize], block_size: usize) -> Vec<usize> {
        // of course this strategy would not need offsets, but we are using them
        // just to have just one, "universal" query search implementation
        let mut block_offsets: Vec<_> = (0..posting_list.len() / block_size)
            .map(|i| i * block_size)
            .collect();

        block_offsets.push(posting_list.len());
        block_offsets
    }

    fn blocking_with_random_kmeans<C:ComponentType,  T: DataType>(
        posting_list: &mut [usize],
        centroid_fraction: f32,
        truncated_kmeans_training: bool,
        _truncation_size: usize,
        min_cluster_size: usize,
        dataset: &SparseDataset<C, T>,
    ) -> Vec<usize> {
        if posting_list.is_empty() {
            return Vec::new();
        }

        let n_centroids = ((centroid_fraction * posting_list.len() as f32) as usize).max(1);
        let mut reordered_posting_list = Vec::<_>::with_capacity(posting_list.len());
        let mut block_offsets = Vec::with_capacity(n_centroids);

        if truncated_kmeans_training {
            // Need to change only how clustering results is computed
            todo!();
        } else {
            let clustering_results =
                do_random_kmeans_on_docids(posting_list, n_centroids, dataset, min_cluster_size);

            block_offsets.push(0);

            for cluster in clustering_results {
                if cluster.is_empty() {
                    continue;
                }
                reordered_posting_list.extend(cluster);
                block_offsets.push(reordered_posting_list.len());
            }

            assert_eq!(reordered_posting_list.len(), posting_list.len());
            posting_list.copy_from_slice(&reordered_posting_list);
        }

        block_offsets
    }

    // ** Summarization strategies **

    fn fixed_size_summary<C, T>(
        dataset: &SparseDataset<C, T>,
        block: &[usize],
        n_components: usize,
    ) -> (Vec<C>, Vec<T>)
    where
        C: ComponentType,
        T: PartialOrd + DataType,
    {
        let mut hash = HashMap::new();
        for &doc_id in block.iter() {
            // for each component_id, store the largest value seen so far
            for (&c, &v) in dataset.iter_vector(doc_id) {
                hash.entry(c)
                    .and_modify(|h| *h = if *h < v { v } else { *h })
                    .or_insert(v);
            }
        }

        let mut components_values: Vec<_> = hash.iter().collect();

        // First sort by decreasing scores, then take only up to LIMIT and sort by component_id
        components_values.sort_unstable_by(|a, b| b.1.partial_cmp(a.1).unwrap());

        components_values.truncate(n_components);

        components_values.sort_unstable_by(|a, b| a.0.cmp(b.0)); // sort by id to make binary search possible

        let components: Vec<_> = components_values
            .iter()
            .map(|(&component_id, _score)| component_id)
            .collect();

        let values: Vec<_> = components.iter().copied().map(|k| hash[&k]).collect();

        (components, values)
    }

    fn energy_preserving_summary<C, T>(
        dataset: &SparseDataset<C,T>,
        block: &[usize],
        fraction: f32,
    ) -> (Vec<C>, Vec<T>)
    where
        C: ComponentType,
        T: PartialOrd + DataType,
    {
        let mut hash = HashMap::new();
        for &doc_id in block.iter() {
            // for each component_id, store the largest value seen so far
            for (&c, &v) in dataset.iter_vector(doc_id) {
                hash.entry(c)
                    .and_modify(|h| *h = if *h < v { v } else { *h })
                    .or_insert(v);
            }
        }

        let mut components_values: Vec<_> = hash.iter().collect();

        components_values.sort_unstable_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        let total_sum = components_values
            .iter()
            .fold(0_f32, |sum, (_, &x)| sum + x.to_f32().unwrap());

        let mut term_ids = Vec::new();
        let mut values = Vec::new();
        let mut acc = 0_f32;
        for (&tid, &v) in components_values.iter() {
            acc += v.to_f32().unwrap();
            term_ids.push(tid);
            values.push(v);
            if (acc / total_sum) > fraction {
                break;
            }
        }
        term_ids.sort();
        let values: Vec<T> = term_ids.iter().copied().map(|k| hash[&k]).collect();
        (term_ids, values)
    }
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
/// Represents the possible choices for the strategy used to prune the posting
/// lists at building time.
/// There are the following possible strategies:
/// - `Fixed  { n_postings: usize }`: Every posting list is pruned by taking its top-`n_postings`
/// - `GlobalThreshold { n_postings: usize, max_fraction: f32 }`: We globally select a threshold and we prune all the postings with smaller score. The threshold is chosen so that every posting list has `n_postings` on average. We limit the number of postings per list to `max_fraction*n_postings`.
pub enum PruningStrategy {
    FixedSize {
        n_postings: usize,
    },
    GlobalThreshold {
        n_postings: usize,
        max_fraction: f32, // limits the length of each posting list to max_fraction*n_postings
    },
}

impl Default for PruningStrategy {
    fn default() -> Self {
        Self::FixedSize { n_postings: 3500 }
    }
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
pub enum BlockingStrategy {
    FixedSize {
        block_size: usize,
    },

    RandomKmeans {
        centroid_fraction: f32,
        truncated_kmeans_training: bool,
        truncation_size: usize,
        min_cluster_size: usize,
    },
}

impl Default for BlockingStrategy {
    fn default() -> Self {
        BlockingStrategy::RandomKmeans {
            centroid_fraction: 0.1,
            truncated_kmeans_training: false,
            truncation_size: 32,
            min_cluster_size: 2,
        }
    }
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
pub enum SummarizationStrategy {
    FixedSize { n_components: usize },
    EnergyPerserving { summary_energy: f32 },
}

impl Default for SummarizationStrategy {
    fn default() -> Self {
        Self::EnergyPerserving {
            summary_energy: 0.4,
        }
    }
}
