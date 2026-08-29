use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::header::{HeaderMap, HeaderValue, HOST};
use serde::Serialize;
use tokio::task::JoinSet;

#[derive(Debug, Parser)]
#[command(about = "Bounded HTTP load generator for microVM validation")]
struct Args {
    #[arg(long)]
    url: String,
    #[arg(long)]
    host: String,
    #[arg(long, default_value_t = 20_000)]
    requests: usize,
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    #[arg(long, default_value_t = 1_000)]
    warmup_requests: usize,
    #[arg(long, default_value_t = 10)]
    timeout_seconds: u64,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    requests: usize,
    successful: usize,
    failed: usize,
    concurrency: usize,
    elapsed_seconds: f64,
    requests_per_second: f64,
    latency_ms: LatencySummary,
}

#[derive(Debug, Serialize)]
struct LatencySummary {
    p50: f64,
    p90: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

#[derive(Debug)]
struct RequestResult {
    latency: Duration,
    successful: bool,
}

fn percentile(sorted: &[Duration], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index].as_secs_f64() * 1_000.0
}

async fn run_requests(
    client: &reqwest::Client,
    url: &str,
    requests: usize,
    concurrency: usize,
) -> Result<Vec<RequestResult>> {
    let mut workers = JoinSet::new();
    for worker in 0..concurrency {
        let client = client.clone();
        let url = url.to_owned();
        let worker_requests = requests / concurrency + usize::from(worker < requests % concurrency);
        workers.spawn(async move {
            let mut results = Vec::with_capacity(worker_requests);
            for _ in 0..worker_requests {
                let started = Instant::now();
                let successful = client
                    .get(&url)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success());
                results.push(RequestResult {
                    latency: started.elapsed(),
                    successful,
                });
            }
            results
        });
    }

    let mut results = Vec::with_capacity(requests);
    while let Some(worker) = workers.join_next().await {
        results.extend(worker.context("HTTP benchmark worker panicked")?);
    }
    Ok(results)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.requests > 0, "--requests must be greater than zero");
    anyhow::ensure!(
        args.concurrency > 0,
        "--concurrency must be greater than zero"
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        HOST,
        HeaderValue::from_str(&args.host).context("invalid --host value")?,
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .pool_max_idle_per_host(args.concurrency)
        .timeout(Duration::from_secs(args.timeout_seconds))
        .build()
        .context("failed to build HTTP client")?;

    if args.warmup_requests > 0 {
        let warmup =
            run_requests(&client, &args.url, args.warmup_requests, args.concurrency).await?;
        anyhow::ensure!(
            warmup.iter().all(|request| request.successful),
            "one or more warm-up requests failed"
        );
    }

    let started = Instant::now();
    let results = run_requests(&client, &args.url, args.requests, args.concurrency).await?;
    let elapsed = started.elapsed();
    let successful = results.iter().filter(|result| result.successful).count();
    let mut latencies: Vec<_> = results.iter().map(|result| result.latency).collect();
    latencies.sort_unstable();

    let output = BenchmarkResult {
        requests: results.len(),
        successful,
        failed: results.len() - successful,
        concurrency: args.concurrency,
        elapsed_seconds: elapsed.as_secs_f64(),
        requests_per_second: results.len() as f64 / elapsed.as_secs_f64(),
        latency_ms: LatencySummary {
            p50: percentile(&latencies, 0.50),
            p90: percentile(&latencies, 0.90),
            p95: percentile(&latencies, 0.95),
            p99: percentile(&latencies, 0.99),
            max: percentile(&latencies, 1.0),
        },
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    anyhow::ensure!(
        output.failed == 0,
        "{} measured requests failed",
        output.failed
    );
    Ok(())
}
