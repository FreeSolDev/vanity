//! Optimized Solana vanity grinder — drop-in replacement for the naive
//! `solana-vanity` crate. Same CLI (`--suffix`) and stdout contract that
//! FreeSolDev/vanity server.js parses (`Address:` / `Private Key (Base58):`
//! / `Time elapsed:`), so the HTTP service is untouched.
//!
//! v0.3: the hot loop lives in `engine.rs` — a batched vartime comb
//! (32 direct-indexed point adds, no doublings, no constant-time scans)
//! with Montgomery batch inversion for compression and a residue-based
//! suffix filter. Seed-correct as before: real 32-byte seeds, pubkey per
//! RFC-8032 (there is no scalar-increment shortcut for Solana's seed-based
//! keypairs). This file keeps the CLI, the dalek reference derivation
//! (`pubkey_from_seed`, now the self-test oracle and >8-char fallback) and
//! the final ed25519-dalek verify gate — a wrong seed→pub can never ship.
//! New flags: `--any-case` (case-insensitive match, ~16x less work for
//! letter suffixes) and `--bench N` (measure keys/s, emit no keys).

mod engine;

use clap::Parser;
use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, scalar::Scalar};
use engine::{Precomp, SuffixFilter, B58, BATCH};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use rayon::prelude::*;
use sha2::{Digest, Sha512};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "Optimized Solana vanity address generator")]
struct Args {
    /// Desired base58 suffix the pubkey must end with (case-sensitive)
    #[arg(short, long)]
    suffix: String,
    /// Worker threads (default: cgroup-aware available parallelism)
    #[arg(short, long, default_value_t = default_threads())]
    threads: usize,
    /// Case-insensitive suffix match — far less work for letter suffixes;
    /// the found address may differ from the request in letter case
    #[arg(long)]
    any_case: bool,
    /// Benchmark: derive N keys, print the keys/s rate, emit no keypair
    #[arg(long, default_value_t = 0)]
    bench: u64,
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
}

/// RFC-8032 ed25519 public key from a 32-byte seed (== Keypair.fromSeed).
#[inline]
fn pubkey_from_seed(seed: &[u8; 32]) -> [u8; 32] {
    let hash = Sha512::digest(seed);
    let mut a = [0u8; 32];
    a.copy_from_slice(&hash[0..32]);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    let scalar = Scalar::from_bits(a);
    let point = &scalar * &ED25519_BASEPOINT_TABLE;
    point.compress().to_bytes()
}

/// True iff base58(pk as big-endian 256-bit int) ends with `suffix`.
/// Computes only the last `suffix.len()` base58 digits (least-significant
/// first = the END of the string), bailing on first mismatch.
#[inline]
fn ends_with_b58(pk: &[u8; 32], suffix: &[u8]) -> bool {
    let mut n = *pk; // big-endian; mutated copy
    for &want in suffix.iter().rev() {
        let mut rem: u32 = 0;
        for b in n.iter_mut() {
            let cur = (rem << 8) | (*b as u32);
            *b = (cur / 58) as u8;
            rem = cur % 58;
        }
        if B58[rem as usize] != want {
            return false;
        }
    }
    true
}

fn main() {
    let args = Args::parse();
    let suffix = args.suffix.into_bytes();
    let any_case = args.any_case;
    if let Err(e) = engine::validate_suffix(&suffix, any_case) {
        eprintln!("Error: {}", e);
        std::process::exit(2);
    }
    let nthreads = args.threads.max(1);

    // Fast path (≤8 chars — i.e. anything practically grindable): batched
    // comb engine, self-tested against the dalek oracle at every start.
    let engine_parts = if suffix.len() <= 8 {
        let pre = Precomp::new();
        engine::self_test(&pre, pubkey_from_seed);
        match SuffixFilter::new(&suffix, any_case) {
            Ok(filter) => Some((pre, filter)),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    let ep = engine_parts.as_ref();

    let found = AtomicBool::new(false);
    let result: Mutex<Option<([u8; 32], [u8; 32])>> = Mutex::new(None);
    let start = Instant::now();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(nthreads)
        .build()
        .expect("rayon pool");

    if args.bench > 0 {
        let (pre, filter) = ep.expect("--bench supports suffixes up to 8 chars");
        let total = AtomicU64::new(0);
        let bench_n = args.bench;
        let bstart = Instant::now();
        pool.install(|| {
            (0..nthreads).into_par_iter().for_each(|tid| {
                let mut os = [0u8; 32];
                getrandom::getrandom(&mut os).expect("os rng");
                os[0] ^= tid as u8;
                os[1] ^= (tid >> 8) as u8;
                let mut rng = ChaCha8Rng::from_seed(os);
                let mut seeds = [[0u8; 32]; BATCH];
                let mut pks = [[0u8; 32]; BATCH];
                loop {
                    rng.fill_bytes(seeds.as_flattened_mut());
                    engine::derive_pubkeys(pre, &seeds, &mut pks);
                    let mut hits = 0u32;
                    for pk in &pks {
                        if filter.matches(pk) {
                            hits += 1;
                        }
                    }
                    std::hint::black_box(hits);
                    let t = total.fetch_add(BATCH as u64, Ordering::Relaxed) + BATCH as u64;
                    if t >= bench_n {
                        break;
                    }
                }
            });
        });
        let elapsed = bstart.elapsed().as_secs_f64();
        let n = total.load(Ordering::Relaxed) as f64;
        println!(
            "Benchmark: {:.0} attempts in {:.3}s = {:.0} keys/s ({} threads)",
            n,
            elapsed,
            n / elapsed,
            nthreads
        );
        return;
    }

    println!(
        "Searching for vanity suffix: \"{}\"{}",
        String::from_utf8_lossy(&suffix),
        if any_case { " (case-insensitive)" } else { "" }
    );
    println!("Using {} threads...", nthreads);

    pool.install(|| {
        (0..nthreads).into_par_iter().for_each(|tid| {
            let mut os = [0u8; 32];
            getrandom::getrandom(&mut os).expect("os rng");
            os[0] ^= tid as u8;
            os[1] ^= (tid >> 8) as u8;
            let mut rng = ChaCha8Rng::from_seed(os);
            if let Some((pre, filter)) = ep {
                let mut seeds = [[0u8; 32]; BATCH];
                let mut pks = [[0u8; 32]; BATCH];
                while !found.load(Ordering::Relaxed) {
                    rng.fill_bytes(seeds.as_flattened_mut());
                    engine::derive_pubkeys(pre, &seeds, &mut pks);
                    for j in 0..BATCH {
                        if filter.matches(&pks[j]) {
                            if !found.swap(true, Ordering::SeqCst) {
                                *result.lock().unwrap() = Some((seeds[j], pks[j]));
                            }
                            return;
                        }
                    }
                }
            } else {
                // Fallback (suffix >8 chars — astronomically infeasible to
                // find, but keep the original behavior): per-attempt path.
                let mut seed = [0u8; 32];
                while !found.load(Ordering::Relaxed) {
                    rng.fill_bytes(&mut seed);
                    let pk = pubkey_from_seed(&seed);
                    let hit = if any_case {
                        engine::reference_matches(&pk, &suffix, true)
                    } else {
                        ends_with_b58(&pk, &suffix)
                    };
                    if hit {
                        if !found.swap(true, Ordering::SeqCst) {
                            *result.lock().unwrap() = Some((seed, pk));
                        }
                        return;
                    }
                }
            }
        });
    });

    let (seed, pk) = result.lock().unwrap().take().expect("no result found");

    // One-time correctness gate via ed25519-dalek v1 (the lib solana-sdk
    // uses): guarantees seed→pubkey matches what Keypair.fromSeed produces.
    {
        let sk = ed25519_dalek::SecretKey::from_bytes(&seed).expect("seed");
        let vp = ed25519_dalek::PublicKey::from(&sk);
        assert_eq!(vp.to_bytes(), pk, "derivation mismatch — refusing to emit");
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!("\nFound a vanity address!");
    println!("Address: {}", bs58::encode(&pk).into_string());
    println!(
        "Private Key (Base58): {}",
        bs58::encode(&seed).into_string()
    );
    println!("Time elapsed: {:.3}", elapsed);
}
