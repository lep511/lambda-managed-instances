# Lambda Managed Instances with Rust

A Rust-based AWS Lambda function that benchmarks CPU-intensive integer factorization on **ARM64 (Graviton4)** instances using **Lambda Managed Instances (LMI)** for multi-concurrency. It exercises deterministic Miller-Rabin primality testing and Pollard's rho factorization (Brent's variant) across a suite of test cases, then reports structured results and CloudWatch metrics.

## Architecture

- **Runtime:** `provided.al2023` (custom runtime, Rust binary named `bootstrap`)
- **Architecture:** `arm64` — cross-compiled for `aarch64-unknown-linux-gnu` with Graviton4-specific tuning (`neoverse-v2`, SVE2, NEON, AES, SHA2/3)
- **Concurrency model:** LMI multi-concurrency via `run_concurrent()` — each invocation runs single-threaded; the Lambda runtime handles multiple concurrent requests as independent Tokio async tasks
- **Memory:** 4096 MB
- **Timeout:** 180 seconds
- **Graceful shutdown:** `spawn_graceful_shutdown_handler()` — registers a SIGTERM hook for cleanup before the execution environment is terminated (~500ms budget)
- **Tracing:** AWS X-Ray (Active)

## Rust-Focused Summary

**What is LMI?**
Lambda Managed Instances runs functions on EC2 instances inside your VPC, allowing multiple concurrent requests per execution environment — unlike standard Lambda's one-request-per-environment model. Announced at re:Invent 2025, with 32 GB / 16 vCPU support added in March 2026.

**Rust's concurrency model in LMI**
Each supported runtime handles multi-concurrency differently, and Rust stands out as the safest option:

| Runtime | Model |
|---|---|
| Python | Multiple processes (fully isolated, simplest) |
| Java | OS threads (shared memory, hardest to get right) |
| Node.js | Worker threads + async |
| .NET | Tasks (async) |
| **Rust** | **Single process, Tokio async tasks** |

Rust uses the **OS-only runtime** and runs handlers as Tokio async tasks within a single process. The key constraint: **handlers must implement `Clone + Send`**. This means the Rust compiler itself enforces thread safety at compile time — bugs that surface at runtime (or in production) in Java or Python are caught before deployment.

**Why Rust is compelling here**
- Zero-cost concurrency safety — the borrow checker prevents shared mutable state by default
- Tokio's async model means high concurrency with minimal memory overhead, unlike Python's per-process memory multiplication (each Python process duplicates the in-memory catalog)
- Well-suited for the compute-heavy pattern of this type of workload (vector math, cosine similarity), where releasing the GIL isn't a concern because there is no GIL
- A Rust similarity engine handler would be naturally concurrent without the memory scaling penalty Python pays

**When LMI makes sense (language-agnostic)**
- Sustained, predictable throughput (hundreds of req/sec)
- Memory-intensive workloads exceeding standard Lambda's 10 GB
- Large in-memory datasets reused across requests (embeddings, models, reference data)
- Cost optimization at scale (10M+ invocations/month)

**Key operational gotchas regardless of language**
- LMI **never scales to zero** — baseline EC2 instances remain running (min ~2–3 instances across AZs)
- Scaling is **asynchronous** and CPU-driven, not request-driven — traffic spikes >2x in 5 minutes risk throttling
- VPC connectivity is mandatory; without it, logs and traces are silently lost
- `publish = true` is required — LMI runs on published versions, not `$LATEST`

**Graceful shutdown lifecycle**

LMI execution environments are long-lived but eventually shut down (scale-in, rebalancing, deployments). The Lambda runtime sends SIGTERM to signal imminent termination, followed by SIGKILL after ~500ms. The `spawn_graceful_shutdown_handler()` function from `lambda_runtime` registers an async closure that executes during this window:

```rust
spawn_graceful_shutdown_handler(|| async {
    tracing::info!("Graceful shutdown initiated");
    // Flush buffered logs, close DB pools, abort multipart uploads, etc.
})
.await;
```

Key details:
- Must be called **before** `run_concurrent()` — it registers an internal no-op Lambda extension (`_lambda-rust-runtime-no-op-graceful-shutdown-helper`) that subscribes to shutdown events
- Requires the `graceful-shutdown` feature flag on `lambda_runtime`
- The closure must complete within ~500ms or the process is forcefully killed via SIGKILL
- Currently logs the shutdown event; extend the closure when adding stateful resources (connection pools, non-blocking log appenders, caches)

## What It Does

Each invocation runs a comprehensive factorization benchmark:

1. **Warmup** — primes CPU caches and branch predictor with repeated factorizations on a single thread.

2. **Category Benchmarks** — sequentially factorizes numbers across five categories:
   - **Large Semiprimes** — products of two large primes (8- to 10-digit factors), the hardest case for Pollard's rho.
   - **Highly Composite** — numbers with many small factors (e.g., `2^63`, `20!`, product of first 15 primes).
   - **Prime Detection** — known large primes up to 18 digits, exercising the deterministic Miller-Rabin test.
   - **Stress Mix** — varied difficulty including multi-prime products, prime powers, and Carmichael numbers.
   - **Edge Cases** — boundary values like the smallest prime, composite Mersenne numbers, and perfect cubes.

3. **Stress Test** — repeatedly factorizes the hardest numbers (500 iterations each) and reports min/max/avg/median/p95/p99 latencies.

4. **Concurrency Readiness Benchmark** — measures single-thread throughput by running 1,600 factorizations of the hardest semiprime, reporting factorizations/sec and average latency per operation. This metric directly indicates how efficiently the function utilizes its LMI concurrency slot.

5. **Observability** — emits structured JSON logs via `tracing` and publishes CloudWatch Embedded Metric Format (EMF) metrics including total time, factorization latency percentiles, single-thread throughput, and average hard-factorization latency.

## Key Algorithms

| Algorithm | Purpose |
|---|---|
| **Deterministic Miller-Rabin** | Primality testing — deterministic for all `u64` values using 12 witnesses |
| **Pollard's rho (Brent variant)** | Factorization of composite numbers — uses cycle detection with GCD batching |
| **Binary GCD** | Fast greatest common divisor using bit shifts (no division) |
| **Modular arithmetic (u128)** | Overflow-safe multiplication and exponentiation via 128-bit intermediates |

## Project Structure

```
.
├── Cargo.toml            # Dependencies and release profile (LTO, single CGU, strip)
├── .cargo/config.toml    # Cross-compilation target and Graviton4 rustflags
├── src/
│   ├── main.rs               # Entrypoint — logging, graceful shutdown hook, run_concurrent()
│   ├── event_handler.rs      # Benchmark handler — factorization, test suites, EMF metrics
│   └── generic_handler.rs    # Example request/response handler (unused, for reference)
├── deploy.sh             # Build, package, and deploy/update the Lambda function
└── test.sh               # Invoke the deployed function in parallel using GNU parallel
```

## Prerequisites

- **Rust** with the `aarch64-unknown-linux-gnu` target installed:
  ```bash
  rustup target add aarch64-unknown-linux-gnu
  ```
- **Cross-compilation linker** for aarch64 (e.g., `gcc-aarch64-linux-gnu`)
- **AWS CLI v2** configured with appropriate permissions
- **GNU parallel** (for `test.sh` only)
- **zip** utility

## Environment Variables

| Variable | Required | Description |
|---|---|---|
| `AWS_REGION` | Yes | AWS region for deployment and invocation |
| `LAMBDA_ROLE_ARN` | Yes (deploy) | IAM execution role ARN for the Lambda function |
| `CAPACITY_PROVIDER_ARN` | Yes (deploy) | ARN of the Lambda managed instances capacity provider |
| `RUST_LOG` | No | Log level filter (default: `info`) |
| `AWS_LAMBDA_MAX_CONCURRENCY` | No | Max concurrent invocations per LMI environment (default: `10`, set by deploy.sh) |

## Build

The `.cargo/config.toml` sets the default target to `aarch64-unknown-linux-gnu` with Graviton4 CPU flags. A standard release build:

```bash
cargo build --release
```

The binary is output to `target/aarch64-unknown-linux-gnu/release/bootstrap`.

### Release Profile Optimizations

- `opt-level = 3` — maximum optimization
- `lto = "fat"` — full cross-crate link-time optimization
- `codegen-units = 1` — single codegen unit for maximum optimization passes
- `strip = true` — strip debug symbols for smaller binary
- `panic = "abort"` — no unwinding overhead

## Deploy

```bash
export AWS_REGION=us-east-1
export LAMBDA_ROLE_ARN=arn:aws:iam::123456789012:role/your-lambda-role
export CAPACITY_PROVIDER_ARN=arn:aws:lambda:us-east-1:123456789012:capacity-provider/your-provider

./deploy.sh
```

The script will:
1. Build the release binary (cross-compiled)
2. Package it into `lambda-function.zip`
3. Create or update the `cpu-optimized-function` Lambda function (with `AWS_LAMBDA_MAX_CONCURRENCY=10`)
4. Publish a new version

## Test

Invoke the deployed function with configurable concurrency:

```bash
export AWS_REGION=us-east-1

# Invoke 2 times in parallel (default)
./test.sh

# Invoke 10 times in parallel
./test.sh 10
```

Invocations are asynchronous (`Event` type). Check **CloudWatch Logs** for benchmark results.

## Run Unit Tests

```bash
cargo test
```

Tests cover primality checking, factorization correctness across all categories, GCD, number formatting, factor verification, and an end-to-end handler invocation.

## CloudWatch Metrics

The function emits the following metrics under the `CpuOptimizedBenchmark` namespace via EMF:

| Metric | Unit | Description |
|---|---|---|
| `TotalTimeMs` | Milliseconds | Total benchmark execution time |
| `TotalFactorizations` | Count | Number of factorizations performed |
| `AvgFactorizationUs` | Microseconds | Average single-factorization latency |
| `P95FactorizationUs` | Microseconds | 95th percentile factorization latency |
| `SingleThreadThroughput` | Count/Second | Factorizations per second on a single thread |
| `AvgHardFactorizationUs` | Microseconds | Average latency for the hardest semiprime factorization |

## Comments on execution

Observations from CloudWatch Logs after deploying version 15 with `run_concurrent()` and `AWS_LAMBDA_MAX_CONCURRENCY=10`.

### LMI infrastructure confirmed

- `initializationType: "lambda-managed-instances"` present in all platform.initStart events
- Init time: **~3.7ms** — the Rust binary starts almost instantly on the OS-only runtime
- 4 LMI instances active across availability zones (4 distinct log streams)
- `instanceMaxMemory: 4,294,967,296` (4 GB as configured)

### Multi-concurrency working

Multiple Tokio worker tasks process invocations simultaneously on the same instance. Observed worker `task_id`s: 7, 8, 9, 11, 15 — confirming the runtime spawned multiple independent polling workers.

Example from instance `4898162374fc4f3a8ecd598b9dcf4d1c`:

| Timestamp | Request | Worker | Event |
|---|---|---|---|
| 23:32:13.926 | c99ac8d2 | task_id=15 | Invocation started |
| 23:32:16.204 | cf21ff8b | task_id=9 | Started while c99ac8d2 still running |
| 23:32:20.245 | c99ac8d2 | task_id=15 | Completed (6,319ms) |
| 23:32:20.882 | 29c00824 | task_id=8 | Invocation started |
| 23:32:22.539 | cf21ff8b | task_id=9 | Completed (6,335ms) |

On another instance, 6 requests arrived within ~720ms and were processed concurrently by different workers.

### Performance consistency under concurrency

Metrics across three concurrent invocations on the same instance show < 0.3% variance:

| Metric | Invocation 1 | Invocation 2 | Invocation 3 |
|---|---|---|---|
| TotalTimeMs | 6,317.84 | 6,332.84 | 6,336.56 |
| SingleThreadThroughput | 344.09/sec | 343.38/sec | 343.40/sec |
| AvgHardFactorizationUs | 2,906.22 | 2,912.22 | 2,912.08 |
| P95FactorizationUs | 1,481.76 | 1,484.25 | 1,478.80 |

Concurrent invocations do not degrade each other — each gets its own CPU slice without significant contention, validating the decision to remove Rayon in favor of LMI-level concurrency.

### Benchmark results by category

| Category | Tests | Time | Latency range |
|---|---|---|---|
| Large Semiprimes | 6 | 9.09ms | 154us - 4,414us |
| Highly Composite | 8 | 0.02ms | 0.23us - 4.14us |
| Prime Detection | 6 | 0.09ms | 4.12us - 20.61us |
| Stress Mix | 8 | 2.02ms | 0.11us - 1,482us |
| Edge Cases | 6 | 0.07ms | 0.05us - 38.75us |

Hardest numbers: `4,294,967,291 x 4,294,967,279` (near 2^32) at 4,414us and `999,999,751 x 999,999,883` (9x9 digit) at 2,894us.

### Stress test latency stability

| Test case | Iterations | Avg | Median | P95 | P99 |
|---|---|---|---|---|---|
| Hardest semiprime (9x9) | 500 | 2,894us | 2,894us | 2,899us | 2,901us |
| 8x8 digit semiprime | 500 | 377us | 376us | 379us | 380us |
| Mersenne M61 (primality) | 500 | 20us | 20us | 20us | 20us |

P99/P50 ratio < 1.01 — virtually zero variance. Graviton4 delivers predictable latency with no jitter.

### Summary

- Zero errors across all log streams
- `all_verified: true` on every invocation — all factorizations mathematically verified
- No core contention under concurrent load, confirming the single-threaded-per-invocation model works well with LMI
- 3,134 total factorizations per invocation completing in ~6.3 seconds

## Estimated Duration

Calculate the cost difference between Lambda Managed Instances and standard Lambda using the CPU-intensive prime factorization workload from this workshop.

**Important Disclaimer**: This cost analysis is based on specific assumptions for this workshop's workload and is provided as a reference example only. Your actual costs will vary based on your workload characteristics, traffic patterns, instance types, regions, and configuration choices. Always calculate costs for your specific use case using the [AWS Pricing Calculator](https://calculator.aws.amazon.com/) or consult your AWS account team for accurate pricing guidance.

## Cost Model Assumptions

This cost analysis is based on the following configuration:

| Configuration Parameter | Value | Impact on Cost |
| --- | --- | --- |
| **EC2 Instance Family** | c7g.xlarge (4 vCPUs, 8 GB) | Determines hourly EC2 cost (~$0.145/hr) |
| **Number of EC2 Instances** | 9 (for 100 concurrent requests) | Baseline cost: 9 × $0.145 × 730 = $952.65/month |
| **Max Concurrency per Environment** | 1 (optimized for CPU) | ~1 per vCPU for optimal performance |
| **Workload Type** | CPU-intensive | Requires low concurrency |
| **Function Memory** | 4096 MB (4 GB) | 2:1 memory-to-vCPU ratio (see Function Deployment chapter) |
| **Duration** | 45s (LMI optimized) / 60s (Standard) | LMI 25% faster with dedicated compute |
| **Peak Concurrency** | 100 concurrent requests | Determines instance count needed |

Concurrency Configuration

**Configuration Note**: For detailed guidance on configuring "Maximum concurrency per execution environment" for CPU-intensive vs I/O-bound workloads, see the Performance Comparison chapter.

## Pricing Model

| Component | Standard Lambda | Lambda Managed Instances |
| --- | --- | --- |
| **Requests** | $0.20 per 1M requests | $0.20 per 1M requests |
| **Compute** | $0.0000133333 per GB-second (ARM) | EC2 instance cost + 15% management fee |
| **Baseline** | $0 (pay per use) | Scales with concurrency needs |

## Cost Comparison: 1 Million Requests/Month

Scenario: 1M requests/month with 100 peak concurrent requests.

**Capacity Planning:**

- 100 concurrent requests ÷ 1 (max concurrency per env) = 100 environments needed
- 100 environments ÷ 12 (envs per c7g.xlarge) = 9 instances

### Standard Lambda Cost

```
Requests: 1M × $0.20/M = $0.20
Compute: 1M × 60s × 4.096 GB × $0.0000133333/GB-s = $3,277.99

Total: $3,278/month
With 17% Compute Savings Plan: $2,721/month
```

### Lambda Managed Instances Cost

```
Requests: 1M × $0.20/M = $0.20
Compute: 9 × $0.145/hr × 730 hrs × 1.15 (15% fee) = $1,095.55

Total: $1,096/month
With 72% EC2 Savings Plan: $412/month
With 75% Reserved Instances: $329/month
```

### Cost Comparison

| Scenario | Standard Lambda | Standard (SP) | LMI | LMI (SP) | LMI (RI) |
| --- | --- | --- | --- | --- | --- |
| **Total** | **$3,278** | **$2,721** | **$1,096** | **$412** | **$329** |
| **Savings** | \- | 17% | 67% | 87% | 90% |

## Scaling to Higher Volumes

| Volume | Concurrent | Instances | Standard | Standard (SP) | LMI (RI) | Savings |
| --- | --- | --- | --- | --- | --- | --- |
| **1M/month** | 100 | 9 | $3,278 | $2,721 | $329 | 90% |
| **10M/month** | 1,000 | 84 | $32,782 | $27,209 | $3,062 | 91% |
| **100M/month** | 10,000 | 834 | $327,820 | $272,091 | $30,405 | 91% |

**Assumptions:** Concurrency scales linearly with volume. Max concurrency = 1 per environment. ~12 environments per c7g.xlarge.

## Break-Even Analysis

**Monthly requests needed to justify LMI (9 instances = $1,096):**

```
Standard Lambda: $0.003278 per request
Break-even: $1,096 / $0.003278 ≈ 334,000 requests/month
```

## Cost Optimization Strategies

| Strategy | Standard Lambda | Lambda Managed Instances |
| --- | --- | --- |
| **Savings Plans** | Compute SP: 17% | EC2 Instance SP: 72% |
| **Reserved Instances** | Not applicable | Up to 75% |
| **Right-sizing** | Adjust memory allocation | Choose optimal instance type |
| **Concurrency Config** | N/A | ~1 per vCPU, tune based on latency SLA |
| **Utilization** | N/A (pay per use) | Keep instances busy (>80%) |

## Key Takeaways

- **90%+ cost savings** for CPU-intensive workloads (>40s duration) with proper configuration
- **Configuration is critical**: See Performance Comparison chapter for concurrency tuning guidance
- **Savings Plans**: 72-75% for LMI vs 17% for standard Lambda
- **Instance count scales with concurrency**: Plan capacity based on peak concurrent requests
