mod harness;

use harness::*;
use messaging::events::Event;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc,
    time::{sleep, timeout},
};

#[derive(Debug, Serialize)]
struct IterationReport {
    iteration: usize,
    app_id: String,
    host: String,
    initial_deploy_and_first_hit_ms: u128,
    probe_attempts_until_ready: u32,
    on_demand_spawn_ms: u128,
    first_request_to_success_ms: u128,
    ready_to_first_success_ms: u128,
    follow_up_request_count: usize,
    follow_up_request_total_ms: u128,
    follow_up_request_avg_ms: f64,
    follow_up_request_p95_ms: u128,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    benchmark: &'static str,
    node_id: String,
    artifact_path: String,
    iterations: usize,
    follow_up_request_count: usize,
    reports: Vec<IterationReport>,
}

#[tokio::test]
#[ignore = "manual benchmark; runs real node/proxy path and writes a report"]
async fn benchmark_platform_cold_start() {
    let iterations = benchmark_iterations();
    let follow_up_request_count = benchmark_follow_up_request_count();

    let nats = NatsContainer::start(4222)
        .await
        .expect("Failed to start NATS");
    let bus = nats.connect().await.expect("Failed to connect to NATS");
    bus.setup_jetstream()
        .await
        .expect("Failed to setup JetStream");
    let (ready_tx, mut ready_rx) = mpsc::unbounded_channel::<(String, Instant)>();
    bus.subscribe("instance.ready.>", move |event| {
        let ready_tx = ready_tx.clone();
        async move {
            if let Event::InstanceReady { app_id, .. } = event {
                let _ = ready_tx.send((app_id.0, Instant::now()));
            }
        }
    })
    .await
    .expect("Failed to subscribe to instance.ready events");

    let node = NodeProcess::start("bench-node-0", &nats.url, 8180, 9000)
        .await
        .expect("Failed to start node");

    let wasm_path = find_hello_axum_wasm().expect("hello-axum.wasm not found");
    let mut reports = Vec::with_capacity(iterations);
    for iteration in 0..iterations {
        let app_id = format!("bench-hello-{iteration}:v1");
        let host = format!("bench-{iteration}.local");
        let config = build_app_config(&app_id, 100_000_000, 100, 2);
        let (artifact_url, sha256, size_bytes, manifests) =
            upload_and_authorize_artifact_for_node(&node, &wasm_path)
                .await
                .expect("Failed to prepare artifact on node artifact server");

        let deploy_started = Instant::now();
        bus.publish(&Event::DeployApp {
            app_id: common::types::AppId(app_id.clone()),
            config,
            artifact_url,
            artifact_transfer_manifests: manifests,
            expected_hash: Some(sha256.clone()),
            size_bytes,
        })
        .await
        .expect("Failed to deploy benchmark app");
        add_route(&bus, &host, &app_id)
            .await
            .expect("Failed to add benchmark route");

        let first_request_started = Instant::now();
        let (first_success_result, instance_ready_result) = tokio::join!(
            wait_for_first_success(node.proxy_port, &host, 60),
            wait_for_instance_ready(&mut ready_rx, &app_id),
        );
        let instance_ready_at =
            instance_ready_result.expect("Timed out waiting for InstanceReady event");
        let (first_request_to_first_success_ms, probe_attempts_until_ready) =
            first_success_result.expect("App did not become ready in time");
        let initial_deploy_and_first_hit_ms = deploy_started.elapsed().as_millis();
        let on_demand_spawn_ms = instance_ready_at
            .duration_since(first_request_started)
            .as_millis();
        let ready_to_first_success_ms =
            first_request_to_first_success_ms.saturating_sub(on_demand_spawn_ms);

        let mut follow_up_latencies_ms = Vec::with_capacity(follow_up_request_count);
        for _ in 0..follow_up_request_count {
            let started = Instant::now();
            let response = send_request(node.proxy_port, &host, "/")
                .await
                .expect("Follow-up request failed");
            assert_eq!(response.status(), 200, "Follow-up request returned non-200");
            let _body = response
                .text()
                .await
                .expect("Failed to read follow-up response");
            follow_up_latencies_ms.push(started.elapsed().as_millis());
            sleep(Duration::from_millis(50)).await;
        }

        let follow_up_request_total_ms = follow_up_latencies_ms.iter().copied().sum::<u128>();
        let follow_up_request_avg_ms =
            follow_up_request_total_ms as f64 / follow_up_latencies_ms.len() as f64;
        let follow_up_request_p95_ms = percentile_95(&mut follow_up_latencies_ms);

        reports.push(IterationReport {
            iteration,
            app_id,
            host,
            initial_deploy_and_first_hit_ms,
            probe_attempts_until_ready,
            on_demand_spawn_ms,
            first_request_to_success_ms: first_request_to_first_success_ms,
            ready_to_first_success_ms,
            follow_up_request_count,
            follow_up_request_total_ms,
            follow_up_request_avg_ms,
            follow_up_request_p95_ms,
        });
    }

    let report = BenchmarkReport {
        benchmark: "platform-cold-start",
        node_id: node.node_id.clone(),
        artifact_path: wasm_path.display().to_string(),
        iterations,
        follow_up_request_count,
        reports,
    };

    emit_report(&report).expect("Failed to emit benchmark report");
    node.stop().ok();
}

fn benchmark_iterations() -> usize {
    std::env::var("COLD_START_BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10)
}

fn benchmark_follow_up_request_count() -> usize {
    std::env::var("COLD_START_BENCH_FOLLOW_UP_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1)
}

async fn wait_for_first_success(
    proxy_port: u16,
    host: &str,
    max_attempts: u32,
) -> Result<(u128, u32), Box<dyn std::error::Error>> {
    let started = Instant::now();
    for attempt in 1..=max_attempts {
        match send_request(proxy_port, host, "/").await {
            Ok(response) if response.status().is_success() => {
                let _body = response.text().await?;
                return Ok((started.elapsed().as_millis(), attempt));
            }
            _ => sleep(Duration::from_millis(250)).await,
        }
    }

    Err("App did not become ready in time".into())
}

async fn wait_for_instance_ready(
    ready_rx: &mut mpsc::UnboundedReceiver<(String, Instant)>,
    app_id: &str,
) -> Result<Instant, Box<dyn std::error::Error>> {
    timeout(Duration::from_secs(30), async {
        while let Some((ready_app_id, ready_at)) = ready_rx.recv().await {
            if ready_app_id == app_id {
                return Ok(ready_at);
            }
        }
        Err("instance.ready subscription closed unexpectedly")
    })
    .await
    .map_err(|_| "Timed out waiting for matching InstanceReady event")?
    .map_err(|e| e.into())
}

fn percentile_95(samples: &mut [u128]) -> u128 {
    samples.sort_unstable();
    let index = ((samples.len() as f64) * 0.95).ceil() as usize;
    let index = index.saturating_sub(1).min(samples.len().saturating_sub(1));
    samples[index]
}

fn emit_report(report: &BenchmarkReport) -> Result<(), Box<dyn std::error::Error>> {
    let output_dir = benchmark_output_dir();
    fs::create_dir_all(&output_dir)?;

    let json_path = output_dir.join("platform_cold_start.json");
    fs::write(&json_path, serde_json::to_vec_pretty(report)?)?;

    let markdown = render_markdown(report);
    let markdown_path = output_dir.join("platform_cold_start.md");
    fs::write(&markdown_path, markdown)?;

    eprintln!("Benchmark report written to {}", json_path.display());
    eprintln!("Benchmark summary written to {}", markdown_path.display());
    Ok(())
}

fn benchmark_output_dir() -> PathBuf {
    std::env::var_os("COLD_START_BENCH_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("workspace root")
                .join("target")
                .join("cold-start-benchmark")
        })
}

fn render_markdown(report: &BenchmarkReport) -> String {
    let avg_initial_deploy_and_first_hit_ms = report
        .reports
        .iter()
        .map(|r| r.initial_deploy_and_first_hit_ms as f64)
        .sum::<f64>()
        / report.reports.len() as f64;
    let avg_on_demand_spawn_ms = report
        .reports
        .iter()
        .map(|r| r.on_demand_spawn_ms as f64)
        .sum::<f64>()
        / report.reports.len() as f64;
    let avg_ready_to_success_ms = report
        .reports
        .iter()
        .map(|r| r.ready_to_first_success_ms as f64)
        .sum::<f64>()
        / report.reports.len() as f64;
    let avg_follow_up_ms = report
        .reports
        .iter()
        .map(|r| r.follow_up_request_avg_ms)
        .sum::<f64>()
        / report.reports.len() as f64;
    let max_initial_deploy_and_first_hit_ms = report
        .reports
        .iter()
        .map(|r| r.initial_deploy_and_first_hit_ms)
        .max()
        .unwrap_or_default();
    let max_follow_up_p95_ms = report
        .reports
        .iter()
        .map(|r| r.follow_up_request_p95_ms)
        .max()
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("# Platform Cold Start Benchmark\n\n");
    out.push_str("- benchmark: real node + proxy path\n");
    out.push_str(&format!("- node: `{}`\n", report.node_id));
    out.push_str(&format!("- artifact: `{}`\n", report.artifact_path));
    out.push_str(&format!("- iterations: `{}`\n", report.iterations));
    out.push_str(&format!(
        "- follow-up requests per iteration: `{}`\n\n",
        report.follow_up_request_count
    ));
    out.push_str("## Summary\n\n");
    out.push_str(&format!(
        "- average initial deploy-and-first-hit: `{avg_initial_deploy_and_first_hit_ms:.2} ms`\n"
    ));
    out.push_str(&format!(
        "- average on-demand spawn: `{avg_on_demand_spawn_ms:.2} ms`\n"
    ));
    out.push_str(&format!(
        "- average ready-to-first-success tail: `{avg_ready_to_success_ms:.2} ms`\n"
    ));
    out.push_str(&format!(
        "- max initial deploy-and-first-hit: `{max_initial_deploy_and_first_hit_ms} ms`\n"
    ));
    out.push_str(&format!(
        "- average follow-up request latency: `{avg_follow_up_ms:.2} ms`\n"
    ));
    out.push_str(&format!(
        "- max follow-up request p95: `{max_follow_up_p95_ms} ms`\n\n"
    ));
    out.push_str("## Per Iteration\n\n");
    out.push_str(
        "| Iteration | Initial Deploy+First Hit ms | Probe Attempts | On-Demand Spawn ms | Ready->First Success Tail ms | Follow-up Avg ms | Follow-up p95 ms |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---:|---:|\n");
    for entry in &report.reports {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {:.2} | {} |\n",
            entry.iteration,
            entry.initial_deploy_and_first_hit_ms,
            entry.probe_attempts_until_ready,
            entry.on_demand_spawn_ms,
            entry.ready_to_first_success_ms,
            entry.follow_up_request_avg_ms,
            entry.follow_up_request_p95_ms
        ));
    }
    out.push_str("\n## Interpretation\n\n");
    out.push_str("- `initial deploy-and-first-hit` is the end-to-end path through deploy event handling, artifact upload/authorization on the node artifact server, compile/store, route registration, on-demand spawn, and the first successful HTTP response.\n");
    out.push_str("- `on-demand spawn` isolates the spawn path after the first request begins and before the instance is marked ready.\n");
    out.push_str("- `ready-to-first-success tail` isolates the remainder of the first request after the instance has registered as ready.\n");
    out.push_str("- `follow-up` metrics measure the immediate post-start request path after the app is already serving.\n");
    out.push_str("- This is not a sustained-load benchmark for the sample app. Use the Wasmtime load review or a dedicated long-lived workload for repeated-request claims.\n");
    out.push_str("- This benchmark does not justify a universal `<10ms cold start` claim. Use the measured numbers from your target Linux environment.\n");
    out
}
