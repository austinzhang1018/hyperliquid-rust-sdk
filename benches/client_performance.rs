use criterion::{ black_box, criterion_group, criterion_main, Criterion };
use ethers::signers::{ LocalWallet, Signer };
use hyperliquid_rust_sdk::{
    BaseUrl,
    ClientCancelRequest,
    ClientLimit,
    ClientOrder,
    ClientOrderRequest,
    ExchangeClient,
    ExchangeDataStatus,
    ExchangeResponseStatus,
    WsExchangeClient,
};
use std::{ sync::Arc, time::{ Duration, Instant } };
use tokio::runtime::Runtime;
use uuid::Uuid;
use dotenv::dotenv;
use std::env;

struct BenchmarkResults {
    http_order_times: Vec<Duration>,
    http_cancel_times: Vec<Duration>,
    ws_order_times: Vec<Duration>,
    ws_cancel_times: Vec<Duration>,
}

async fn setup_http_client() -> ExchangeClient {
    // Use a private key for testing on testnet
    let private_key = env
        ::var("HYPERLIQUID_SECRET_KEY")
        .expect("HYPERLIQUID_SECRET_KEY env variable not set");

    let wallet = private_key.parse::<LocalWallet>().expect("Failed to parse private key");

    // Create exchange client on testnet
    ExchangeClient::new(None, wallet, Some(BaseUrl::Testnet), None, None).await.expect(
        "Failed to create HTTP exchange client"
    )
}

async fn setup_ws_client() -> WsExchangeClient {
    // Use the same private key for testing on testnet
    let private_key = env
        ::var("HYPERLIQUID_SECRET_KEY")
        .expect("HYPERLIQUID_SECRET_KEY env variable not set");

    let wallet = private_key.parse::<LocalWallet>().expect("Failed to parse private key");

    // Create websocket exchange client on testnet
    WsExchangeClient::new(
        wallet,
        Some(BaseUrl::Testnet),
        None,
        None,
        true // Enable reconnect
    ).await.expect("Failed to create WebSocket exchange client")
}

async fn benchmark_http_client(runs: u32) -> (Vec<Duration>, Vec<Duration>) {
    let client = setup_http_client().await;
    let mut order_times = Vec::with_capacity((runs as usize) * 3);
    let mut cancel_times = Vec::with_capacity((runs as usize) * 3);

    // Benchmark multiple runs
    for _ in 0..runs {
        // Run 3 iterations of place and cancel
        for _ in 0..3 {
            // Create a small order for ETH
            let cloid = Some(Uuid::new_v4());
            let order_req = ClientOrderRequest {
                asset: "ETH".to_string(),
                is_buy: true,
                reduce_only: false,
                limit_px: 1000.0, // Price - doesn't matter since we cancel immediately
                sz: 0.01, // Very small size for testing
                cloid: cloid.clone(),
                order_type: ClientOrder::Limit(ClientLimit {
                    tif: "Gtc".to_string(),
                }),
            };

            // Place order and measure time
            let start = Instant::now();
            let order_result = client.order(order_req, None).await;
            let order_duration = start.elapsed();
            order_times.push(order_duration);

            // Make sure order went through
            if let Err(e) = order_result {
                eprintln!("HTTP order error: {:?}", e);
                continue;
            }

            let mut oid = 0;

            let res = order_result.unwrap();
            if let ExchangeResponseStatus::Ok(res) = res {
                if let ExchangeDataStatus::Resting(status) = &res.data.unwrap().statuses[0] {
                    oid = status.oid;
                } else {
                    panic!("Unexpected response!");
                }
            } else {
                panic!("Order failed: {:?}", res);
            }

            // Cancel order and measure time
            let cancel_req = ClientCancelRequest {
                asset: "ETH".to_string(),
                oid: oid, // We'll use cloid instead
            };

            let start = Instant::now();
            let cancel_result = client.cancel(cancel_req, None).await;
            let cancel_duration = start.elapsed();
            cancel_times.push(cancel_duration);

            if let Err(e) = cancel_result {
                eprintln!("HTTP cancel error: {:?}", e);
            }

            // Wait a bit before next iteration
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        // Wait before next run
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    (order_times, cancel_times)
}

async fn benchmark_ws_client(runs: u32) -> (Vec<Duration>, Vec<Duration>) {
    let client = setup_ws_client().await;
    let mut order_times = Vec::with_capacity((runs as usize) * 3);
    let mut cancel_times = Vec::with_capacity((runs as usize) * 3);

    // Benchmark multiple runs
    for _ in 0..runs {
        // Run 3 iterations of place and cancel
        for _ in 0..3 {
            // Create a small order for ETH
            let cloid = Some(Uuid::new_v4());
            let order_req = ClientOrderRequest {
                asset: "ETH".to_string(),
                is_buy: true,
                reduce_only: false,
                limit_px: 1000.0, // Price - doesn't matter since we cancel immediately
                sz: 0.01, // Very small size for testing
                cloid: cloid.clone(),
                order_type: ClientOrder::Limit(ClientLimit {
                    tif: "Gtc".to_string(),
                }),
            };

            // Place order and measure time
            let start = Instant::now();
            let order_result = client.bulk_order(vec![order_req], None).await;
            let order_duration = start.elapsed();
            order_times.push(order_duration);

            // Make sure order went through
            if let Err(e) = order_result {
                eprintln!("WS order error: {:?}", e);
                continue;
            }

            let mut oid = 0;
            if let ExchangeResponseStatus::Ok(res) = order_result.unwrap() {
                if let ExchangeDataStatus::Resting(status) = &res.data.unwrap().statuses[0] {
                    oid = status.oid;
                } else {
                    panic!("Unexpected response!");
                }
            } else {
                panic!("Order failed!");
            }

            // Cancel order and measure time
            let cancel_req = ClientCancelRequest {
                asset: "ETH".to_string(),
                oid: oid, // We'll use cloid instead
            };
            let start = Instant::now();
            let cancel_result = client.bulk_cancel(vec![cancel_req], None).await;
            let cancel_duration = start.elapsed();
            cancel_times.push(cancel_duration);

            if let Err(e) = cancel_result {
                eprintln!("WS cancel error: {:?}", e);
            }

            // Wait a bit before next iteration
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        // Wait before next run
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    (order_times, cancel_times)
}

fn format_duration_stats(durations: &[Duration]) -> String {
    if durations.is_empty() {
        return "No data".to_string();
    }

    let total: Duration = durations.iter().sum();
    let avg = total / (durations.len() as u32);

    let min = durations.iter().min().unwrap();
    let max = durations.iter().max().unwrap();

    format!("avg: {:?}, min: {:?}, max: {:?}, total ops: {}", avg, min, max, durations.len())
}

fn run_benchmark(c: &mut Criterion) {
    // Load environment variables from .env file if present
    dotenv().ok();

    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("exchange_client_comparison");
    group.sample_size(10); // We'll manually handle 10 runs inside our benchmark function

    group.bench_function("http_vs_ws_client", |b| {
        b.iter(|| {
            // Run the actual benchmark
            let results = rt.block_on(async {
                let runs = 10; // 10 runs of 3 operations each

                println!("Running WebSocket client benchmark...");
                let (ws_order_times, ws_cancel_times) = benchmark_ws_client(runs).await;

                println!("Running HTTP client benchmark...");
                let (http_order_times, http_cancel_times) = benchmark_http_client(runs).await;

                BenchmarkResults {
                    http_order_times,
                    http_cancel_times,
                    ws_order_times,
                    ws_cancel_times,
                }
            });

            // Print the results
            println!("\n===== BENCHMARK RESULTS =====");
            println!(
                "HTTP Client - Order operations: {}",
                format_duration_stats(&results.http_order_times)
            );
            println!(
                "HTTP Client - Cancel operations: {}",
                format_duration_stats(&results.http_cancel_times)
            );
            println!(
                "WS Client - Order operations: {}",
                format_duration_stats(&results.ws_order_times)
            );
            println!(
                "WS Client - Cancel operations: {}",
                format_duration_stats(&results.ws_cancel_times)
            );

            // Calculate averages for comparison
            if !results.http_order_times.is_empty() && !results.ws_order_times.is_empty() {
                let http_avg_order: Duration =
                    results.http_order_times.iter().sum::<Duration>() /
                    (results.http_order_times.len() as u32);
                let ws_avg_order: Duration =
                    results.ws_order_times.iter().sum::<Duration>() /
                    (results.ws_order_times.len() as u32);

                if http_avg_order > ws_avg_order {
                    let speedup = http_avg_order.as_secs_f64() / ws_avg_order.as_secs_f64();
                    println!("WebSocket is {:.2}x faster for orders", speedup);
                } else {
                    let speedup = ws_avg_order.as_secs_f64() / http_avg_order.as_secs_f64();
                    println!("HTTP is {:.2}x faster for orders", speedup);
                }
            }

            if !results.http_cancel_times.is_empty() && !results.ws_cancel_times.is_empty() {
                let http_avg_cancel: Duration =
                    results.http_cancel_times.iter().sum::<Duration>() /
                    (results.http_cancel_times.len() as u32);
                let ws_avg_cancel: Duration =
                    results.ws_cancel_times.iter().sum::<Duration>() /
                    (results.ws_cancel_times.len() as u32);

                if http_avg_cancel > ws_avg_cancel {
                    let speedup = http_avg_cancel.as_secs_f64() / ws_avg_cancel.as_secs_f64();
                    println!("WebSocket is {:.2}x faster for cancels", speedup);
                } else {
                    let speedup = ws_avg_cancel.as_secs_f64() / http_avg_cancel.as_secs_f64();
                    println!("HTTP is {:.2}x faster for cancels", speedup);
                }
            }

            // Return something for criterion
            black_box(())
        });
    });

    group.finish();
}

criterion_group!(benches, run_benchmark);
criterion_main!(benches);
