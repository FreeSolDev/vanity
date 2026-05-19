//! Optimized Solana vanity grinder — drop-in replacement for the naive
//! `solana-vanity` crate. Same CLI (`--suffix`) and stdout contract that
//! FreeSolDev/vanity server.js parses (`Address:` / `Private Key (Base58):`
//! / `Time elapsed:`), so the HTTP service is untouched.
//!
//! Why it's faster on the SAME cores (no point-addition trick — Solana
//! keypairs are seed-based, so we must grind real 32-byte seeds and derive
//! the pubkey per RFC-8032):
//!   1. ChaCha8 CSPRNG seeded ONCE per worker (vs OsRng syscall per attempt).
//!   2. Fixed-base scalar mult via ED25519_BASEPOINT_TABLE (vs the generic
//!      mult inside solana-sdk's Keypair::new()), no per-attempt struct/alloc.
//!   3. Trailing-base58-digit early reject: the last k base58 chars are just
//!      k cheap divmod-by-58 steps on the 256-bit pubkey — reject ~57/58 of
//!      attempts WITHOUT a full base58 encode + String alloc.
//! Output keypair is verified once (ed25519-dalek v1, the lib solana-sdk
//! uses) before printing — a wrong seed→pub can never ship.

use clap::Parser;
use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, scalar::Scalar};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use rayon::prelude::*;
use sha2::{Digest, Sha512};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const B58: &[u8; 58] =
    b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

#[derive(Parser)]
#[command(about = "Optimized Solana vanity address generator")]
struct Args {
    /// Desired base58 suffix the pubkey must end with (case-sensitive)
    #[arg(short, long)]
    suffix: String,
    /// Worker threads (default: cgroup-aware available parallelism)
    #[arg(short, long, default_value_t = default_threads())]
    threads: usize,
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
    if suffix.is_empty() || !suffix.iter().all(|c| B58.contains(c)) {
        eprintln!("Error: suffix must be non-empty valid base58 chars");
        std::process::exit(2);
    }
    let nthreads = args.threads.max(1);
    println!("Searching for vanity suffix: \"{}\"", String::from_utf8_lossy(&suffix));
    println!("Using {} threads...", nthreads);

    let found = AtomicBool::new(false);
    let result: Mutex<Option<([u8; 32], [u8; 32])>> = Mutex::new(None);
    let start = Instant::now();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(nthreads)
        .build()
        .expect("rayon pool");
    pool.install(|| {
        (0..nthreads).into_par_iter().for_each(|tid| {
            let mut os = [0u8; 32];
            getrandom::getrandom(&mut os).expect("os rng");
            os[0] ^= tid as u8;
            os[1] ^= (tid >> 8) as u8;
            let mut rng = ChaCha8Rng::from_seed(os);
            let mut seed = [0u8; 32];
            while !found.load(Ordering::Relaxed) {
                rng.fill_bytes(&mut seed);
                let pk = pubkey_from_seed(&seed);
                if ends_with_b58(&pk, &suffix) {
                    if !found.swap(true, Ordering::SeqCst) {
                        *result.lock().unwrap() = Some((seed, pk));
                    }
                    return;
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
