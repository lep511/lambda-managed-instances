use lambda_runtime::{Error, LambdaEvent};
use serde_json::Value;
use rayon::prelude::*;
use serde::Serialize;
use std::hint::black_box;
use std::time::Instant;
use tracing;

// ── Result types ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct FactorizationResult {
    label: String,
    number: u64,
    number_display: String,
    factors: Vec<u64>,
    factorization_display: String,
    is_prime: bool,
    num_prime_factors: usize,
    difficulty: String,
    verified: bool,
    duration_us: f64,
}

#[derive(Serialize)]
struct BenchmarkCategory {
    category: String,
    description: String,
    num_tests: usize,
    results: Vec<FactorizationResult>,
    category_time_ms: f64,
}

#[derive(Serialize)]
struct StressTestResult {
    label: String,
    number: u64,
    number_display: String,
    expected_factors: String,
    iterations: usize,
    total_ms: f64,
    avg_us: f64,
    min_us: f64,
    max_us: f64,
    median_us: f64,
    p95_us: f64,
    p99_us: f64,
}

#[derive(Serialize)]
struct ParallelBenchmark {
    description: String,
    total_factorizations: usize,
    sequential_ms: f64,
    parallel_ms: f64,
    speedup_factor: f64,
    threads_used: usize,
    efficiency_percent: f64,
    verdict: String,
}

#[derive(Serialize)]
struct SummaryStats {
    fastest_us: f64,
    slowest_us: f64,
    avg_us: f64,
    median_us: f64,
    p95_us: f64,
}

#[derive(Serialize)]
struct Summary {
    total_unique_numbers: usize,
    total_factorizations: usize,
    all_verified: bool,
    total_time_ms: f64,
    single_factorization_stats: SummaryStats,
}

// ── Utilities ────────────────────────────────────────────────────────────────

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

fn factors_display(factors: &[u64]) -> String {
    if factors.is_empty() {
        return String::new();
    }
    let mut groups: Vec<(u64, usize)> = Vec::new();
    for &f in factors {
        if let Some(last) = groups.last_mut() {
            if last.0 == f {
                last.1 += 1;
                continue;
            }
        }
        groups.push((f, 1));
    }
    groups
        .iter()
        .map(|&(base, exp)| {
            if exp == 1 {
                format_number(base)
            } else {
                format!("{}^{}", format_number(base), exp)
            }
        })
        .collect::<Vec<_>>()
        .join(" × ")
}

fn verify_factors(n: u64, factors: &[u64]) -> bool {
    factors
        .iter()
        .try_fold(1u64, |acc, &f| acc.checked_mul(f))
        == Some(n)
}

// ── Modular arithmetic (u128 intermediates avoid overflow on Graviton) ───────

#[inline(always)]
fn mod_mul(a: u64, b: u64, m: u64) -> u64 {
    ((a as u128 * b as u128) % m as u128) as u64
}

#[inline(always)]
fn mod_pow(mut base: u64, mut exp: u64, modulus: u64) -> u64 {
    let mut result = 1u64;
    base %= modulus;
    while exp > 0 {
        if exp & 1 == 1 {
            result = mod_mul(result, base, modulus);
        }
        exp >>= 1;
        base = mod_mul(base, base, modulus);
    }
    result
}

#[inline(always)]
fn gcd(mut a: u64, mut b: u64) -> u64 {
    if a == 0 {
        return b;
    }
    if b == 0 {
        return a;
    }
    let shift = (a | b).trailing_zeros();
    a >>= a.trailing_zeros();
    loop {
        b >>= b.trailing_zeros();
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        b -= a;
        if b == 0 {
            return a << shift;
        }
    }
}

// ── Miller-Rabin primality test (deterministic for all u64) ──────────────────

const MR_WITNESSES: &[u64] = &[2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37];

fn is_prime(n: u64) -> bool {
    if n < 2 {
        return false;
    }
    for &p in MR_WITNESSES {
        if n == p {
            return true;
        }
        if n % p == 0 {
            return false;
        }
    }

    let mut d = n - 1;
    let mut r = 0u32;
    while d & 1 == 0 {
        d >>= 1;
        r += 1;
    }

    'witness: for &a in MR_WITNESSES {
        let mut x = mod_pow(a, d, n);
        if x == 1 || x == n - 1 {
            continue;
        }
        for _ in 0..r - 1 {
            x = mod_mul(x, x, n);
            if x == n - 1 {
                continue 'witness;
            }
        }
        return false;
    }
    true
}

// ── Brent's variant of Pollard's rho ─────────────────────────────────────────

fn pollard_brent(n: u64, c: u64) -> u64 {
    let f = |x: u64| (mod_mul(x, x, n) + c) % n;

    let mut y = 2u64;
    let mut r = 1u64;
    let mut q = 1u64;
    let mut d;
    let mut x;
    #[allow(unused_assignments)]
    let mut ys = 0u64;

    loop {
        x = y;
        for _ in 0..r {
            y = f(y);
        }
        let mut k = 0u64;
        loop {
            ys = y;
            let steps = r.saturating_sub(k).min(128);
            for _ in 0..steps {
                y = f(y);
                q = mod_mul(q, x.abs_diff(y), n);
            }
            d = gcd(q, n);
            k += 128;
            if k >= r || d != 1 {
                break;
            }
        }
        r *= 2;
        if d != 1 {
            break;
        }
    }

    if d == n {
        loop {
            ys = f(ys);
            d = gcd(x.abs_diff(ys), n);
            if d != 1 {
                break;
            }
        }
    }
    d
}

// ── Recursive factorization ──────────────────────────────────────────────────

fn factorize(n: u64, factors: &mut Vec<u64>) {
    if n <= 1 {
        return;
    }
    if is_prime(n) {
        factors.push(n);
        return;
    }

    for &p in &[2u64, 3, 5, 7, 11, 13, 17, 19, 23] {
        if n % p == 0 {
            factors.push(p);
            factorize(n / p, factors);
            return;
        }
    }

    let mut c = 1u64;
    loop {
        let d = pollard_brent(n, c);
        if d != n {
            factorize(d, factors);
            factorize(n / d, factors);
            return;
        }
        c += 1;
    }
}

// ── Difficulty classifier ────────────────────────────────────────────────────

fn classify_difficulty(factors: &[u64]) -> String {
    if factors.len() == 1 {
        return "prime — Miller-Rabin deterministic test".to_string();
    }
    let max_factor = *factors.iter().max().unwrap_or(&0);
    let digits = max_factor.to_string().len();
    if max_factor < 1_000 {
        format!("trivial (largest factor: {} digits)", digits)
    } else if max_factor < 1_000_000 {
        format!("easy (largest factor: {} digits)", digits)
    } else if max_factor < 1_000_000_000 {
        format!("medium (largest factor: {} digits)", digits)
    } else {
        format!("hard (largest factor: {} digits)", digits)
    }
}

// ── Prime factorization with rich output ─────────────────────────────────────

fn prime_factorization(label: &str, n: u64) -> FactorizationResult {
    let start = Instant::now();
    let mut factors = Vec::new();
    factorize(n, &mut factors);
    factors.sort_unstable();
    let duration_us = start.elapsed().as_secs_f64() * 1_000_000.0;
    let prime = factors.len() == 1 && factors[0] == n;
    let difficulty = classify_difficulty(&factors);
    let display = factors_display(&factors);
    let verified = verify_factors(n, &factors);
    FactorizationResult {
        label: label.to_string(),
        number: n,
        number_display: format_number(n),
        factorization_display: display,
        num_prime_factors: factors.len(),
        is_prime: prime,
        difficulty,
        verified,
        factors,
        duration_us: round2(duration_us),
    }
}

// ── Test data ────────────────────────────────────────────────────────────────

struct TestCase {
    label: &'static str,
    number: u64,
}

fn category_large_semiprimes() -> Vec<TestCase> {
    // All constructed as verified p × q where p and q are known primes
    vec![
        TestCase {
            label: "99,999,971 × 99,999,989 (8-digit × 8-digit)",
            number: 99_999_971u64 * 99_999_989,
        },
        TestCase {
            label: "999,999,937 × 999,999,893 (9-digit × 9-digit)",
            number: 999_999_937u64 * 999_999_893,
        },
        TestCase {
            label: "999,999,751 × 999,999,883 (9-digit × 9-digit)",
            number: 999_999_751u64 * 999_999_883,
        },
        TestCase {
            label: "9,999,991 × 9,999,973 (7-digit × 7-digit)",
            number: 9_999_991u64 * 9_999_973,
        },
        TestCase {
            label: "99,999,989² (perfect square semiprime)",
            number: 99_999_989u64 * 99_999_989,
        },
        TestCase {
            label: "4,294,967,291 × 4,294,967,279 (near 2^32)",
            number: 4_294_967_291u64 * 4_294_967_279,
        },
    ]
}

fn category_highly_composite() -> Vec<TestCase> {
    vec![
        TestCase {
            label: "2^40 (40 prime factors)",
            number: 1u64 << 40,
        },
        TestCase {
            label: "2^50 (50 prime factors)",
            number: 1u64 << 50,
        },
        TestCase {
            label: "2^63 (63 prime factors)",
            number: 1u64 << 63,
        },
        TestCase {
            label: "20! (factorial, 36 prime factors)",
            number: 2_432_902_008_176_640_000,
        },
        TestCase {
            label: "2^3 × 3^3 × 5^3 × 7^3",
            number: 2u64.pow(3) * 3u64.pow(3) * 5u64.pow(3) * 7u64.pow(3),
        },
        TestCase {
            label: "720,720 (highly composite, 10 factors)",
            number: 720_720,
        },
        TestCase {
            label: "Product of first 15 primes",
            number: 2 * 3 * 5 * 7 * 11 * 13 * 17 * 19 * 23 * 29 * 31 * 37 * 41 * 43 * 47,
        },
        TestCase {
            label: "2^20 × 3^10 × 7^3",
            number: 2u64.pow(20) * 3u64.pow(10) * 7u64.pow(3),
        },
    ]
}

fn category_primes() -> Vec<TestCase> {
    vec![
        TestCase {
            label: "10^16 − 63 (16 digits)",
            number: 9_999_999_999_999_937,
        },
        TestCase {
            label: "10^15 − 11 (15 digits)",
            number: 999_999_999_999_989,
        },
        TestCase {
            label: "2^31 − 1 = Mersenne prime M31",
            number: 2_147_483_647,
        },
        TestCase {
            label: "10^12 + 39 (13 digits)",
            number: 1_000_000_000_039,
        },
        TestCase {
            label: "~10^18 (18 digits, verified prime)",
            number: 999_999_999_999_999_877,
        },
        TestCase {
            label: "2^61 − 1 = Mersenne prime M61",
            number: 2_305_843_009_213_693_951,
        },
    ]
}

fn category_stress_mixed() -> Vec<TestCase> {
    vec![
        TestCase {
            label: "97 × 99,989 × 99,991 (3 primes)",
            number: 97u64 * 99_989 * 99_991,
        },
        TestCase {
            label: "29 × (10^15 − 11) (small × large prime)",
            number: 29u64 * 999_999_999_999_989,
        },
        TestCase {
            label: "7^19 (large prime power)",
            number: 7u64.pow(19),
        },
        TestCase {
            label: "RSA-like: 99,999,959 × 99,999,941",
            number: 99_999_959u64 * 99_999_941,
        },
        TestCase {
            label: "M31² = (2^31 − 1)² (Mersenne squared)",
            number: 2_147_483_647u64 * 2_147_483_647,
        },
        TestCase {
            label: "Fibonacci(44) = 701,408,733",
            number: 701_408_733,
        },
        TestCase {
            label: "10^18 = 2^18 × 5^18 (power of 10)",
            number: 1_000_000_000_000_000_000,
        },
        TestCase {
            label: "Carmichael number: 561 = 3 × 11 × 17",
            number: 561,
        },
    ]
}

fn category_edge_cases() -> Vec<TestCase> {
    vec![
        TestCase {
            label: "Smallest prime: 2",
            number: 2,
        },
        TestCase {
            label: "Largest single-digit prime: 7",
            number: 7,
        },
        TestCase {
            label: "Large Carmichael: 41,041 = 7 × 11 × 13 × 41",
            number: 41_041,
        },
        TestCase {
            label: "10^16 − 64 (adjacent to known prime)",
            number: 9_999_999_999_999_936,
        },
        TestCase {
            label: "2^59 − 1 (composite Mersenne)",
            number: (1u64 << 59) - 1,
        },
        TestCase {
            label: "Perfect cube: 9,261³ = 3³ × 7³ × 7³... (deep recursion)",
            number: 9_261u64 * 9_261 * 9_261,
        },
    ]
}


// ── EMF metric helper ────────────────────────────────────────────────────────

fn emit_emf_metrics(
    namespace: &str,
    dimensions: &[(&str, &str)],
    metrics: &[(&str, f64, &str)],
) {
    let dim_keys: Vec<&str> = dimensions.iter().map(|(k, _)| *k).collect();
    let dim_values: serde_json::Map<String, Value> = dimensions
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
    let metric_defs: Vec<Value> = metrics
        .iter()
        .map(|(name, _, unit)| {
            serde_json::json!({ "Name": name, "Unit": unit })
        })
        .collect();
    let mut blob = serde_json::json!({
        "_aws": {
            "Timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            "CloudWatchMetrics": [{
                "Namespace": namespace,
                "Dimensions": [dim_keys],
                "Metrics": metric_defs,
            }]
        }
    });
    // Merge dimensions and metric values into the top-level object
    let obj = blob.as_object_mut().unwrap();
    for (k, v) in &dim_values {
        obj.insert(k.clone(), v.clone());
    }
    for (name, value, _) in metrics {
        obj.insert(name.to_string(), serde_json::json!(value));
    }
    // EMF blobs must be printed directly to stdout (not through tracing)
    println!("{}", serde_json::to_string(&blob).unwrap());
}

// ── Main handler ─────────────────────────────────────────────────────────────
pub(crate)async fn function_handler(event: LambdaEvent<Value>) -> Result<(), Error> {
    let request_id = event.context.request_id.clone();
    let function_name = event.context.env_config.function_name.clone();

    // Extract some useful information from the request
    let _payload = event.payload;
    tracing::info!(request_id = %request_id, %function_name, "Invocation started");

    let threads = num_cpus::get();
    let total_start = Instant::now();

    // Warmup: initialize Rayon thread pool and warm CPU caches
    let warmup_n = 99_999_971u64 * 99_999_989;
    (0..threads).into_par_iter().for_each(|_| {
        for _ in 0..20 {
            let mut f = Vec::new();
            factorize(warmup_n, &mut f);
            black_box(&f);
        }
    });

    // ── 1. Run categories (sequential iteration for accurate per-test timing) ──

    let category_defs: Vec<(&str, &str, Vec<TestCase>)> = vec![
        (
            "Large Semiprimes",
            "Products of two large primes (verified constructions) — hardest case for Pollard's rho",
            category_large_semiprimes(),
        ),
        (
            "Highly Composite",
            "Numbers with many small factors — tests recursive decomposition and trial division",
            category_highly_composite(),
        ),
        (
            "Prime Detection",
            "Known large primes — exercises deterministic Miller-Rabin (no factorization)",
            category_primes(),
        ),
        (
            "Stress Mix",
            "Varied difficulty: multi-prime products, powers, Carmichael numbers, edge values",
            category_stress_mixed(),
        ),
        (
            "Edge Cases",
            "Boundary values: tiny primes, composite Mersenne, perfect cubes, adjacent primes",
            category_edge_cases(),
        ),
    ];

    let categories: Vec<BenchmarkCategory> = category_defs
        .into_iter()
        .map(|(name, desc, cases)| {
            let cat_start = Instant::now();
            let results: Vec<FactorizationResult> = cases
                .iter()
                .map(|tc| prime_factorization(tc.label, tc.number))
                .collect();
            BenchmarkCategory {
                category: name.to_string(),
                description: desc.to_string(),
                num_tests: results.len(),
                category_time_ms: round2(cat_start.elapsed().as_secs_f64() * 1000.0),
                results,
            }
        })
        .collect();

    // ── 2. Stress test: repeated factorization with per-iteration statistics ───

    let stress_cases: Vec<(&str, u64, usize)> = vec![
        (
            "Hardest semiprime (9×9 digit factors)",
            999_999_751u64 * 999_999_883,
            500,
        ),
        (
            "8×8 digit semiprime",
            99_999_971u64 * 99_999_989,
            500,
        ),
        (
            "Mersenne prime M61 (primality only)",
            2_305_843_009_213_693_951,
            500,
        ),
    ];

    let stress_test: Vec<StressTestResult> = stress_cases
        .into_iter()
        .map(|(label, n, iterations)| {
            let mut timings_us = Vec::with_capacity(iterations);
            let total_start = Instant::now();
            for _ in 0..iterations {
                let iter_start = Instant::now();
                let mut factors = Vec::new();
                factorize(n, &mut factors);
                black_box(&factors);
                timings_us.push(iter_start.elapsed().as_secs_f64() * 1_000_000.0);
            }
            let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
            timings_us.sort_by(|a, b| a.partial_cmp(b).unwrap());

            let mut factors = Vec::new();
            factorize(n, &mut factors);
            factors.sort_unstable();

            StressTestResult {
                label: label.to_string(),
                number: n,
                number_display: format_number(n),
                expected_factors: factors_display(&factors),
                iterations,
                total_ms: round2(total_ms),
                avg_us: round2(timings_us.iter().sum::<f64>() / iterations as f64),
                min_us: round2(timings_us[0]),
                max_us: round2(*timings_us.last().unwrap()),
                median_us: round2(percentile(&timings_us, 50.0)),
                p95_us: round2(percentile(&timings_us, 95.0)),
                p99_us: round2(percentile(&timings_us, 99.0)),
            }
        })
        .collect();

    // ── 3. Parallel benchmark: enough work per task to show real speedup ───────

    let hard_number = 999_999_751u64 * 999_999_883;
    let batches = 8usize;
    let iters_per_batch = 200usize;
    let total_iters = batches * iters_per_batch;

    // Sequential: all batches one after another
    let seq_start = Instant::now();
    for _ in 0..total_iters {
        let mut factors = Vec::new();
        factorize(hard_number, &mut factors);
        black_box(&factors);
    }
    let sequential_ms = seq_start.elapsed().as_secs_f64() * 1000.0;

    // Parallel: batches distributed across threads
    let par_start = Instant::now();
    (0..batches).into_par_iter().for_each(|_| {
        for _ in 0..iters_per_batch {
            let mut factors = Vec::new();
            factorize(hard_number, &mut factors);
            black_box(&factors);
        }
    });
    let parallel_ms = par_start.elapsed().as_secs_f64() * 1000.0;

    let speedup = if parallel_ms > 0.0 {
        sequential_ms / parallel_ms
    } else {
        0.0
    };
    let efficiency = speedup / threads as f64 * 100.0;

    let verdict = if speedup >= 1.5 {
        format!(
            "Excellent: {:.2}x speedup with {} threads ({:.0}% efficient)",
            speedup, threads, efficiency
        )
    } else if speedup >= 1.1 {
        format!(
            "Good: {:.2}x speedup with {} threads ({:.0}% efficient)",
            speedup, threads, efficiency
        )
    } else if speedup >= 0.9 {
        "Neutral: parallelism overhead roughly equals computation gain".to_string()
    } else {
        format!(
            "Overhead-bound: {:.2}x — tasks too small for {} threads to help",
            speedup, threads
        )
    };

    let parallel_benchmark = ParallelBenchmark {
        description: format!(
            "{} batches × {} iterations of hardest semiprime = {} total factorizations",
            batches, iters_per_batch, total_iters
        ),
        total_factorizations: total_iters,
        sequential_ms: round2(sequential_ms),
        parallel_ms: round2(parallel_ms),
        speedup_factor: round2(speedup),
        threads_used: threads,
        efficiency_percent: round2(efficiency),
        verdict,
    };

    // ── 4. Summary statistics ──────────────────────────────────────────────────

    let all_durations: Vec<f64> = categories
        .iter()
        .flat_map(|cat| cat.results.iter().map(|r| r.duration_us))
        .collect();
    let all_verified = categories
        .iter()
        .flat_map(|cat| cat.results.iter())
        .all(|r| r.verified);
    let total_unique = all_durations.len();
    let stress_iters: usize = stress_test.iter().map(|s| s.iterations).sum();
    let total_factorizations = total_unique + stress_iters + total_iters;
    let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;

    let mut sorted = all_durations.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let summary = Summary {
        total_unique_numbers: total_unique,
        total_factorizations,
        all_verified,
        total_time_ms: round2(total_ms),
        single_factorization_stats: SummaryStats {
            fastest_us: round2(sorted.first().copied().unwrap_or(0.0)),
            slowest_us: round2(sorted.last().copied().unwrap_or(0.0)),
            avg_us: round2(all_durations.iter().sum::<f64>() / total_unique as f64),
            median_us: round2(percentile(&sorted, 50.0)),
            p95_us: round2(percentile(&sorted, 95.0)),
        },
    };

    // ── 5. Structured logs ────────────────────────────────────────────────────

    tracing::info!(
        request_id = %request_id,
        threads_available = threads,
        "Benchmark started"
    );

    for cat in &categories {
        tracing::info!(
            request_id = %request_id,
            category = %cat.category,
            num_tests = cat.num_tests,
            category_time_ms = cat.category_time_ms,
            "Category completed"
        );
        for r in &cat.results {
            tracing::info!(
                request_id = %request_id,
                category = %cat.category,
                label = %r.label,
                number = r.number,
                factorization = %r.factorization_display,
                is_prime = r.is_prime,
                verified = r.verified,
                duration_us = r.duration_us,
                difficulty = %r.difficulty,
                "Factorization result"
            );
        }
    }

    for s in &stress_test {
        tracing::info!(
            request_id = %request_id,
            label = %s.label,
            iterations = s.iterations,
            avg_us = s.avg_us,
            median_us = s.median_us,
            p95_us = s.p95_us,
            p99_us = s.p99_us,
            total_ms = s.total_ms,
            "Stress test completed"
        );
    }

    tracing::info!(
        request_id = %request_id,
        total_factorizations = parallel_benchmark.total_factorizations,
        sequential_ms = parallel_benchmark.sequential_ms,
        parallel_ms = parallel_benchmark.parallel_ms,
        speedup_factor = parallel_benchmark.speedup_factor,
        threads_used = parallel_benchmark.threads_used,
        efficiency_percent = parallel_benchmark.efficiency_percent,
        verdict = %parallel_benchmark.verdict,
        "Parallel benchmark completed"
    );

    tracing::info!(
        request_id = %request_id,
        total_unique_numbers = summary.total_unique_numbers,
        total_factorizations = summary.total_factorizations,
        all_verified = summary.all_verified,
        total_time_ms = summary.total_time_ms,
        fastest_us = summary.single_factorization_stats.fastest_us,
        slowest_us = summary.single_factorization_stats.slowest_us,
        avg_us = summary.single_factorization_stats.avg_us,
        median_us = summary.single_factorization_stats.median_us,
        p95_us = summary.single_factorization_stats.p95_us,
        "Benchmark summary"
    );

    // ── 6. Emit CloudWatch EMF metrics ──────────────────────────────────────

    emit_emf_metrics(
        "CpuOptimizedBenchmark",
        &[("FunctionName", &function_name)],
        &[
            ("TotalTimeMs", summary.total_time_ms, "Milliseconds"),
            ("TotalFactorizations", summary.total_factorizations as f64, "Count"),
            ("AvgFactorizationUs", summary.single_factorization_stats.avg_us, "Microseconds"),
            ("P95FactorizationUs", summary.single_factorization_stats.p95_us, "Microseconds"),
            ("ParallelSpeedup", parallel_benchmark.speedup_factor, "None"),
            ("ThreadsUsed", parallel_benchmark.threads_used as f64, "Count"),
            ("ParallelEfficiency", parallel_benchmark.efficiency_percent, "Percent"),
        ],
    );

    tracing::info!(request_id = %request_id, "Invocation completed");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_prime() {
        assert!(!is_prime(0));
        assert!(!is_prime(1));
        assert!(is_prime(2));
        assert!(is_prime(3));
        assert!(!is_prime(4));
        assert!(is_prime(2_147_483_647)); // M31
        assert!(is_prime(2_305_843_009_213_693_951)); // M61
        assert!(is_prime(9_999_999_999_999_937));
        assert!(!is_prime(561)); // Carmichael number
    }

    #[test]
    fn test_factorize_primes() {
        for p in [2, 3, 7, 2_147_483_647, 9_999_999_999_999_937] {
            let mut factors = Vec::new();
            factorize(p, &mut factors);
            assert_eq!(factors, vec![p], "prime {} should have itself as only factor", p);
        }
    }

    #[test]
    fn test_factorize_powers_of_two() {
        let mut factors = Vec::new();
        factorize(1u64 << 40, &mut factors);
        factors.sort_unstable();
        assert_eq!(factors, vec![2; 40]);
    }

    #[test]
    fn test_factorize_semiprimes() {
        let cases: Vec<(u64, Vec<u64>)> = vec![
            (99_999_971u64 * 99_999_989, vec![99_999_971, 99_999_989]),
            (999_999_937u64 * 999_999_893, vec![999_999_893, 999_999_937]),
            (9_999_991u64 * 9_999_973, vec![9_999_973, 9_999_991]),
        ];
        for (n, expected) in cases {
            let mut factors = Vec::new();
            factorize(n, &mut factors);
            factors.sort_unstable();
            assert_eq!(factors, expected, "factorize({}) failed", n);
        }
    }

    #[test]
    fn test_factorize_highly_composite() {
        // 720720 = 2^4 × 3^2 × 5 × 7 × 11 × 13
        let mut factors = Vec::new();
        factorize(720_720, &mut factors);
        factors.sort_unstable();
        assert_eq!(factors, vec![2, 2, 2, 2, 3, 3, 5, 7, 11, 13]);
    }

    #[test]
    fn test_verify_factors() {
        let n = 999_999_937u64 * 999_999_893;
        let mut factors = Vec::new();
        factorize(n, &mut factors);
        assert!(verify_factors(n, &factors), "product of factors must equal n");
    }

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(1_000_000), "1,000,000");
        assert_eq!(format_number(42), "42");
        assert_eq!(format_number(999_999_937), "999,999,937");
    }

    #[test]
    fn test_gcd() {
        assert_eq!(gcd(12, 8), 4);
        assert_eq!(gcd(0, 5), 5);
        assert_eq!(gcd(7, 0), 7);
        assert_eq!(gcd(17, 13), 1);
    }

    #[tokio::test]
    async fn test_event_is_blank() {
        let payload = serde_json::Value::Null;
        let context = lambda_runtime::Context::default();
        let event = LambdaEvent::new(payload, context);
        function_handler(event).await.expect("handler should succeed with blank event");
    }

    #[test]
    fn test_all_categories_verify() {
        let all_cases: Vec<TestCase> = category_large_semiprimes()
            .into_iter()
            .chain(category_highly_composite())
            .chain(category_primes())
            .chain(category_stress_mixed())
            .chain(category_edge_cases())
            .collect();

        for tc in &all_cases {
            let result = prime_factorization(tc.label, tc.number);
            assert!(result.verified, "{}: factors {:?} don't multiply to {}", tc.label, result.factors, tc.number);
        }
    }
}

