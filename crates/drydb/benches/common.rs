//! Shared harness for the benchmarks.
//!
//! Deliberately small: a timer, a percentile summary and an allocation counter. The
//! numbers a run prints are only comparable against another run on the same machine with
//! the same toolchain, so every report says what it measured rather than implying a
//! universal figure.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
}

fn record(size: usize) {
    let _ = COUNTING.try_with(|on| {
        if on.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
            let _ = ALLOCATED_BYTES.try_with(|n| n.set(n.get() + size as u64));
        }
    });
}

/// Forwards to the system allocator and counts what the measuring thread asks for.
pub struct CountingAllocator;

// SAFETY: every method forwards to the system allocator unchanged.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// What one measured run produced.
pub struct Measurement {
    pub label: String,
    pub operations: u64,
    pub elapsed: Duration,
    pub allocations: u64,
    pub allocated_bytes: u64,
    pub percentiles: Option<Percentiles>,
    pub notes: Vec<String>,
}

pub struct Percentiles {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
}

impl Measurement {
    pub fn per_second(&self) -> f64 {
        if self.elapsed.as_secs_f64() == 0.0 {
            return 0.0;
        }
        self.operations as f64 / self.elapsed.as_secs_f64()
    }

    pub fn print(&self) {
        print!(
            "  {:<44} {:>12.0} op/s  {:>9.2?}/op  {:>7} allocs",
            self.label,
            self.per_second(),
            self.elapsed / self.operations.max(1) as u32,
            self.allocations
        );
        if let Some(p) = &self.percentiles {
            print!(
                "  p50 {:>8.2?}  p95 {:>8.2?}  p99 {:>8.2?}",
                p.p50, p.p95, p.p99
            );
        }
        for note in &self.notes {
            print!("  {note}");
        }
        println!();
    }
}

/// Times `body`, which returns how many operations it performed.
pub fn measure(label: &str, body: impl FnOnce() -> u64) -> Measurement {
    ALLOCATIONS.with(|n| n.set(0));
    ALLOCATED_BYTES.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    let start = Instant::now();
    let operations = body();
    let elapsed = start.elapsed();
    COUNTING.with(|c| c.set(false));
    Measurement {
        label: label.to_string(),
        operations,
        elapsed,
        allocations: ALLOCATIONS.with(|n| n.get()),
        allocated_bytes: ALLOCATED_BYTES.with(|n| n.get()),
        percentiles: None,
        notes: Vec::new(),
    }
}

/// Times each iteration separately, so the tail of the distribution is visible.
pub fn measure_each(
    label: &str,
    iterations: usize,
    mut body: impl FnMut(usize) -> u64,
) -> Measurement {
    let mut samples = Vec::with_capacity(iterations);
    ALLOCATIONS.with(|n| n.set(0));
    ALLOCATED_BYTES.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    let start = Instant::now();
    let mut operations = 0u64;
    for i in 0..iterations {
        let iteration = Instant::now();
        operations += body(i);
        samples.push(iteration.elapsed());
    }
    let elapsed = start.elapsed();
    COUNTING.with(|c| c.set(false));

    samples.sort_unstable();
    let percentile = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    Measurement {
        label: label.to_string(),
        operations,
        elapsed,
        allocations: ALLOCATIONS.with(|n| n.get()),
        allocated_bytes: ALLOCATED_BYTES.with(|n| n.get()),
        percentiles: Some(Percentiles {
            p50: percentile(0.50),
            p95: percentile(0.95),
            p99: percentile(0.99),
        }),
        notes: Vec::new(),
    }
}

/// A deterministic generator, so two runs measure the same work.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 17) ^ (self.0 >> 33)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Reads a scale factor from the environment, so a quick run and a thorough one use the
/// same code.
pub fn scale(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn header(title: &str) {
    println!();
    println!("{title}");
    println!("{}", "-".repeat(title.len()));
}

pub fn environment() {
    println!("drydb benchmarks");
    println!(
        "  target {} / {} / profile {}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        if cfg!(debug_assertions) {
            "debug (numbers are not meaningful)"
        } else {
            "release"
        }
    );
    println!(
        "  parallelism {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    if cfg!(debug_assertions) {
        println!("  run with `cargo bench` or `--release`; a debug build measures the checks, not the code");
    }
}
