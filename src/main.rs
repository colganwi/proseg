#![allow(confusable_idents)]

use clap::Parser;

mod sampler;

use sampler::{Sampler, ModelPriors, ModelParams, ProposalStats};
use sampler::transcripts::{read_transcripts_csv, neighborhood_graph, coordinate_span, Transcript};
use sampler::hexbinsampler::HexBinSampler;
use rayon::current_num_threads;
use csv;
use std::fs::File;
use flate2::Compression;
use flate2::write::GzEncoder;

// use signal_hook::{consts::SIGINT, iterator::Signals};
// use std::{error::Error, thread, time::Duration};


#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args{
    transcript_csv: String,
    // cell_centers_csv: String,

    #[arg(long, default_value="feature_name")]
    transcript_column: String,

    #[arg(long, default_value="x_location")]
    x_column: String,

    #[arg(long, default_value="y_location")]
    y_column: String,

    #[arg(short, long, default_value=None)]
    z_column: Option<String>,

    #[arg(long, default_value="x_centroid")]
    cell_x_column: String,

    #[arg(long, default_value="y_centroid")]
    cell_y_column: String,

    #[arg(short, long, default_value_t=20)]
    ncomponents: usize,

    #[arg(long, default_value_t=100000)]
    niter: usize,

    #[arg(short = 't', long, default_value=None)]
    nthreads: Option<usize>,

    #[arg(short, long, default_value_t=0.05_f32)]
    background_prob: f32,

    #[arg(short, long, default_value_t=100)]
    local_steps_per_iter: usize,

    #[arg(short, long, default_value="counts.csv.gz")]
    output_counts: String,
}


fn main() {
    // let mut signals = Signals::new(&[SIGINT])?;

    // thread::spawn(move || {
    //     for sig in signals.forever() {
    //         panic!();
    //     }
    // });

    let args = Args::parse();

    assert!(args.ncomponents > 0);

    let (transcript_names, transcripts, init_cell_assignments, init_cell_population) = read_transcripts_csv(
        &args.transcript_csv, &args.transcript_column, &args.x_column,
        &args.y_column, args.z_column.as_deref());
    let ngenes = transcript_names.len();
    let ntranscripts = transcripts.len();
    let ncells = init_cell_population.len() - 1;

    println!("Read {} transcripts", ntranscripts);

    let full_area = sampler::hull::compute_full_area(&transcripts);
    println!("Full area: {}", full_area);

    // let nuclei_centroids = read_nuclei_csv(
    //     &args.cell_centers_csv, &args.cell_x_column, &args.cell_y_column);
    // let ncells = nuclei_centroids.len();

    let (xmin, xmax, ymin, ymax) = coordinate_span(&transcripts);
    let (xspan, yspan) = (xmax - xmin, ymax - ymin);

    if let Some(nthreads) = args.nthreads {
        rayon::ThreadPoolBuilder::new().num_threads(nthreads).build_global().unwrap();
    }
    let nthreads = current_num_threads();
    println!("Using {} threads", nthreads);

    // Find a reasonable grid size to use to chunk the data
    const CHUNK_FACTOR: usize = 4;
    let area = (xmax - xmin) * (ymax - ymin);
    let mut chunk_size = (area / ((nthreads * CHUNK_FACTOR) as f32)).sqrt();

    let min_cells_per_chunk = (ncells as f64).min(100.0);

    let nchunks = |chunk_size: f32, xspan: f32, yspan: f32| {
        ((xspan / chunk_size).ceil() as usize) * ((yspan / chunk_size).ceil() as usize)
    };

    while (ncells as f64) / (nchunks(chunk_size, xspan, yspan) as f64) < min_cells_per_chunk {
        chunk_size *= std::f32::consts::SQRT_2;
    }

    // while (ncells as f64) / ((grid_size * grid_size) as f64) < min_cells_per_chunk {
    //     grid_size *= std::f32::consts::SQRT_2;
    // }
    println!("Using grid size {}. Chunks: {}", chunk_size, nchunks(chunk_size, xspan, yspan));

    let quadrant_size = chunk_size / 2.0;
    let (adjacency, transcript_areas, avg_edge_length) =
        neighborhood_graph(&transcripts, quadrant_size);

    println!("Built neighborhood graph with {} edges", adjacency.edge_count()/2);

    // can't just divide area by number of cells, because a large portion may have to cells.

    let priors = ModelPriors {
        min_cell_area: avg_edge_length,
        μ_μ_a: (avg_edge_length * avg_edge_length * (ntranscripts as f32) / (ncells as f32)).ln(),
        σ_μ_a: 3.0_f32,
        α_σ_a: 0.1,
        β_σ_a: 0.1,
        // α_w: 1.0,
        // β_w: 1.0,
        α_θ: 1.0,
        β_θ: 1.0,
        e_r: 1.0,
        f_r: 1.0,
    };

    let mut params = ModelParams::new(
        &priors,
        full_area,
        &transcripts,
        &init_cell_assignments,
        &init_cell_population,
        &transcript_areas,
        args.ncomponents,
        ncells,
        ngenes
    );

    // TODO: Need to somehow make this a command line argument.
    // Maybe just set the total number of iterations
    let sampler_schedule = [
        (5.0_f32, 200),
        (2.5_f32, 200),
        (1.0_f32, 200),
        (0.5_f32, 200),
    ];


    for (avghexpop, niter) in sampler_schedule.iter() {
        println!("Running sampler with avghexpop: {}, niter: {}", avghexpop, niter);
        run_hexbin_sampler(
            &priors,
            &mut params,
            &transcripts,
            ncells,
            ngenes,
            chunk_size,
            full_area,
            *avghexpop,
            *niter,
            args.local_steps_per_iter);
    }

    {
        let file = File::create(&args.output_counts).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(encoder);

        writer.write_record(transcript_names.iter()).unwrap();
        for row in params.counts.t().rows() {
            writer.write_record(row.iter().map(|x| x.to_string())).unwrap();
        }
    }

    // TODO: dumping component assignments for debugging
    {
        let file = File::create("z.csv.gz").unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(encoder);

        writer.write_record(["z"]).unwrap();
        for z in params.z.iter() {
            writer.write_record([z.to_string()]).unwrap();
        }
    }

    params.write_cell_hulls(&transcripts, "cells.geojson.gz");

    {
        let file = File::create("cell_assignments.csv.gz").unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(encoder);

        writer.write_record(["x", "y", "gene", "assignment"]).unwrap();
        for (cell, transcript) in params.cell_assignments.iter().zip(&transcripts) {
            writer.write_record([
                transcript.x.to_string(),
                transcript.y.to_string(),
                transcript_names[transcript.gene as usize].clone(),
                cell.to_string().to_string()]).unwrap();
        }
    }

}


fn run_hexbin_sampler(
        priors: &ModelPriors,
        params: &mut ModelParams,
        transcripts: &Vec<Transcript>,
        ncells: usize,
        ngenes: usize,
        chunk_size: f32,
        full_area: f32,
        avghexpop: f32,
        niter: usize,
        local_steps_per_iter: usize)
{
    let mut sampler = HexBinSampler::new(
        priors,
        params,
        transcripts,
        ncells,
        ngenes,
        full_area,
        avghexpop,
        chunk_size
    );

    sampler.sample_global_params(priors, params);
    let mut proposal_stats = ProposalStats::new();

    for i in 0..niter {
        for _ in 0..local_steps_per_iter {
            sampler.sample_cell_regions(priors, params, &mut proposal_stats, transcripts);
        }
        sampler.sample_global_params(priors, params);

        println!("Log likelihood: {}", params.log_likelihood());

        // dbg!(&proposal_stats);
        proposal_stats.reset();

        if i % 100 == 0 {
            println!("Iteration {} ({} unassigned transcripts)", i, params.nunassigned());
        }
    }
}
