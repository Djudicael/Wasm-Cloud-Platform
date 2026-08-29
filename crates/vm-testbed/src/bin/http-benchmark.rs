use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, HOST};
use reqwest::Method;
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
    #[arg(long, default_value = "GET")]
    method: Method,
    #[arg(long, conflicts_with = "body_file")]
    body: Option<String>,
    #[arg(long, conflicts_with = "body")]
    body_file: Option<PathBuf>,
    #[arg(long)]
    content_type: Option<String>,
    #[arg(long = "expected-status", value_delimiter = ',')]
    expected_statuses: Vec<u16>,
    #[arg(long)]
    rate_per_second: Option<f64>,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    requests: usize,
    successful: usize,
    failed: usize,
    concurrency: usize,
    method: String,
    target_rate_per_second: Option<f64>,
    elapsed_seconds: f64,
    requests_per_second: f64,
    status_counts: BTreeMap<String, usize>,
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
    status: Option<u16>,
}

#[derive(Clone, Debug)]
struct RequestContract {
    method: Method,
    body: Option<String>,
    expected_statuses: Vec<u16>,
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
    contract: &RequestContract,
    rate_per_second: Option<f64>,
) -> Result<Vec<RequestResult>> {
    let mut workers = JoinSet::new();
    let load_started = tokio::time::Instant::now();
    for worker in 0..concurrency {
        let client = client.clone();
        let url = url.to_owned();
        let contract = contract.clone();
        workers.spawn(async move {
            let worker_requests =
                requests / concurrency + usize::from(worker < requests % concurrency);
            let mut results = Vec::with_capacity(worker_requests);
            for request_index in (worker..requests).step_by(concurrency) {
                if let Some(rate) = rate_per_second {
                    let scheduled =
                        load_started + Duration::from_secs_f64(request_index as f64 / rate);
                    tokio::time::sleep_until(scheduled).await;
                }
                let started = Instant::now();
                let mut request = client.request(contract.method.clone(), &url);
                if let Some(body) = contract.body.as_ref() {
                    request = request.body(body.clone());
                }
                let status = request.send().await.ok().map(|response| response.status());
                let successful = status.is_some_and(|status| {
                    if contract.expected_statuses.is_empty() {
                        status.is_success()
                    } else {
                        contract.expected_statuses.contains(&status.as_u16())
                    }
                });
                results.push(RequestResult {
                    latency: started.elapsed(),
                    successful,
                    status: status.map(|status| status.as_u16()),
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
    if let Some(rate) = args.rate_per_second {
        anyhow::ensure!(
            rate.is_finite() && rate > 0.0,
            "--rate-per-second must be finite and greater than zero"
        );
    }
    let body = match args.body_file.as_ref() {
        Some(path) => Some(
            tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("failed to read request body from {}", path.display()))?,
        ),
        None => args.body.clone(),
    };
    let contract = RequestContract {
        method: args.method.clone(),
        body,
        expected_statuses: args.expected_statuses.clone(),
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        HOST,
        HeaderValue::from_str(&args.host).context("invalid --host value")?,
    );
    if let Some(content_type) = args.content_type.as_ref() {
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_str(content_type).context("invalid --content-type value")?,
        );
    }
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .pool_max_idle_per_host(args.concurrency)
        .timeout(Duration::from_secs(args.timeout_seconds))
        .build()
        .context("failed to build HTTP client")?;

    if args.warmup_requests > 0 {
        let warmup = run_requests(
            &client,
            &args.url,
            args.warmup_requests,
            args.concurrency,
            &contract,
            args.rate_per_second,
        )
        .await?;
        anyhow::ensure!(
            warmup.iter().all(|request| request.successful),
            "one or more warm-up requests failed"
        );
    }

    let started = Instant::now();
    let results = run_requests(
        &client,
        &args.url,
        args.requests,
        args.concurrency,
        &contract,
        args.rate_per_second,
    )
    .await?;
    let elapsed = started.elapsed();
    let successful = results.iter().filter(|result| result.successful).count();
    let mut latencies: Vec<_> = results.iter().map(|result| result.latency).collect();
    latencies.sort_unstable();
    let mut status_counts = BTreeMap::new();
    for result in &results {
        let status = result
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "transport_error".to_string());
        *status_counts.entry(status).or_insert(0) += 1;
    }

    let output = BenchmarkResult {
        requests: results.len(),
        successful,
        failed: results.len() - successful,
        concurrency: args.concurrency,
        method: args.method.to_string(),
        target_rate_per_second: args.rate_per_second,
        elapsed_seconds: elapsed.as_secs_f64(),
        requests_per_second: results.len() as f64 / elapsed.as_secs_f64(),
        status_counts,
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
