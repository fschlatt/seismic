use seismic::inverted_index::{
    BlockingStrategy, Configuration, PruningStrategy, SummarizationStrategy,
};
use seismic::{InvertedIndex, SparseDataset};

use std::fs;

use clap::Parser;
use std::time::Instant;

// TODO:
// - add control to the Rayon's number of threads

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// The path of the input file
    #[clap(short, long, value_parser)]
    input_file: Option<String>,

    /// The path of the output file. The extension will encode the values of thebuilding parameters.
    #[clap(short, long, value_parser)]
    output_file: Option<String>,

    /// The number of postings to be selected in each posting list.
    #[clap(short, long, value_parser)]
    #[arg(default_value_t = 6000)]
    n_postings: usize,

    /// Block size in the fixed size blockin
    #[clap(short, long, value_parser)]
    #[arg(default_value_t = 10)]
    block_size: usize,

    /// Regulates the number of centroids built for each posting list. The number of centroids is at most the fraction of the posting list lenght.
    #[clap(long, value_parser)]
    #[arg(default_value_t = 0.1)]
    centroid_fraction: f32,

    #[clap(short, long, value_parser)]
    #[arg(default_value_t = 0.5)]
    summary_energy: f32,

    #[clap(short, long, value_parser)]
    #[arg(default_value_t = false)]
    truncation: bool,

    #[clap(short, long, value_parser)]
    #[arg(default_value_t = 16)]
    truncation_size: usize,

    #[clap(short, long, value_parser)]
    #[arg(default_value_t = 2)]
    min_cluster_size: usize,
}

pub fn main() {
    let args = Args::parse();

    let dataset = SparseDataset::<u16, f32>::read_bin_file(&args.input_file.unwrap())
        .unwrap()
        .quantize_f16();

    println!("Number of Vectors: {}", dataset.len());
    println!("Number of Dimensions: {}", dataset.dim());

    println!(
        "Avg number of components: {:.2}",
        dataset.nnz() as f32 / dataset.len() as f32
    );

    let time = Instant::now();

    let config = Configuration::default()
        .pruning_strategy(PruningStrategy::GlobalThreshold {
            n_postings: args.n_postings,
            max_fraction: 1.5,
        })
        .blocking_strategy(BlockingStrategy::RandomKmeans {
            centroid_fraction: args.centroid_fraction,
            truncated_kmeans_training: args.truncation,
            truncation_size: args.truncation_size,
            min_cluster_size: args.min_cluster_size,
        })
        .summarization_strategy(SummarizationStrategy::EnergyPerserving {
            summary_energy: args.summary_energy,
        });
    println!("\nBuilding the index...");
    println!("{:?}", config);

    let inverted_index = InvertedIndex::build(dataset, config);

    let elapsed = time.elapsed();
    println!(
        "Time to build {} secs (before serializing)",
        elapsed.as_secs()
    );
    let serialized = bincode::serialize(&inverted_index).unwrap();

    let path = args.output_file.unwrap() + ".index.seismic";

    println!("Saving ... {}", path);
    let r = fs::write(path, serialized);
    println!("{:?}", r);

    let elapsed = time.elapsed();
    println!("Time to build {} secs", elapsed.as_secs());
}
