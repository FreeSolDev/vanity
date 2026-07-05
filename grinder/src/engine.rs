//! Batched vartime ed25519 keygen engine for vanity grinding.
//!
//! Replaces the per-attempt dalek path (constant-time fixed-base mult +
//! one field inversion PER KEY inside `compress()`) with:
//!
//!   1. **Vartime signed radix-256 comb**: pubkey = Σ d_i·(2^{8i}·B) with
//!      d_i ∈ [-128,128], i < 32. That's 32 direct-indexed table loads +
//!      32 mixed additions, ZERO doublings and no constant-time select
//!      scans. Constant time buys nothing here: candidates are random and
//!      discarded by the million; the winning key is re-verified and never
//!      externally timed.
//!   2. **Montgomery batch inversion**: compress `BATCH` points with ONE
//!      inversion + 3 muls/point instead of one inversion per point.
//!   3. **Residue suffix filter**: "base58 ends with suffix" ⇔
//!      N mod 58^k ∈ precomputed target set — a single Horner pass over the
//!      32 pubkey bytes instead of k full divmod sweeps.
//!
//! Field arithmetic is fiat-crypto (formally verified, 51-bit × 5 limbs).
//! Point formulas mirror curve25519-dalek's unified a=-1 twisted Edwards
//! formulas (add-2008-hwcd-3 / dbl-2008-hwcd), which are complete on the
//! curve — safe even for accidental P+P / P+(-P).
//!
//! Correctness is gated at runtime, every process start, by `self_test()`:
//! 256 seeds derived by this engine must match the dalek oracle in main.rs
//! bit-for-bit, and the residue filter must agree with a reference divmod
//! matcher on real bs58-encoded addresses (positive and negative cases).
//! On any mismatch the process aborts before grinding. The winning key is
//! additionally re-derived via ed25519-dalek in main.rs before printing.

use fiat_crypto::curve25519_64::*;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use sha2::{Digest, Sha512};

/// Keys derived (and compressed together) per batch. 128 keeps all batch
/// state in L1/L2 while amortizing the single inversion to ~2 muls/key.
pub const BATCH: usize = 128;

pub const B58: &[u8; 58] =
    b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

// ---------------------------------------------------------------------------
// Field elements (GF(2^255-19)) — thin wrapper over fiat-crypto.
// `Fe` = tight (fully carried) bounds, `FeL` = loose bounds. fiat's contract:
// mul/square eat loose, produce tight; add/sub/opp eat tight, produce loose.
// Tight limbs always satisfy loose bounds, so `relax` is a plain copy.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Fe([u64; 5]);
#[derive(Clone, Copy)]
pub struct FeL([u64; 5]);

#[inline(always)]
fn tight(v: &[u64; 5]) -> fiat_25519_tight_field_element {
    fiat_25519_tight_field_element(*v)
}
#[inline(always)]
fn loose(v: &[u64; 5]) -> fiat_25519_loose_field_element {
    fiat_25519_loose_field_element(*v)
}

impl Fe {
    pub const ZERO: Fe = Fe([0; 5]);
    pub const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    #[inline(always)]
    pub fn relax(&self) -> FeL {
        FeL(self.0)
    }
    pub fn from_bytes(b: &[u8; 32]) -> Fe {
        let mut o = tight(&[0; 5]);
        fiat_25519_from_bytes(&mut o, b);
        Fe(o.0)
    }
    #[inline(always)]
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        fiat_25519_to_bytes(&mut b, &tight(&self.0));
        b
    }
    #[inline(always)]
    pub fn add(a: &Fe, b: &Fe) -> FeL {
        let mut o = loose(&[0; 5]);
        fiat_25519_add(&mut o, &tight(&a.0), &tight(&b.0));
        FeL(o.0)
    }
    #[inline(always)]
    pub fn sub(a: &Fe, b: &Fe) -> FeL {
        let mut o = loose(&[0; 5]);
        fiat_25519_sub(&mut o, &tight(&a.0), &tight(&b.0));
        FeL(o.0)
    }
    #[inline(always)]
    pub fn neg(a: &Fe) -> Fe {
        let mut o = loose(&[0; 5]);
        fiat_25519_opp(&mut o, &tight(&a.0));
        FeL(o.0).carry()
    }
    #[inline(always)]
    pub fn sq(&self) -> Fe {
        self.relax().sq()
    }
    fn pow2k(&self, k: u32) -> Fe {
        let mut y = *self;
        for _ in 0..k {
            y = y.sq();
        }
        y
    }
    /// z^(p-2) = z^(2^255 - 21) — classic ref10 chain (254 sq + 11 mul).
    pub fn invert(&self) -> Fe {
        let z = self.relax();
        let t0 = self.sq(); // 2
        let t1 = t0.pow2k(2); // 8
        let t2 = FeL::mul(&z, &t1.relax()); // 9
        let t3 = FeL::mul(&t0.relax(), &t2.relax()); // 11
        let t4 = t3.sq(); // 22
        let t5 = FeL::mul(&t2.relax(), &t4.relax()); // 2^5-1
        let t6 = FeL::mul(&t5.pow2k(5).relax(), &t5.relax()); // 2^10-1
        let t7 = FeL::mul(&t6.pow2k(10).relax(), &t6.relax()); // 2^20-1
        let t8 = FeL::mul(&t7.pow2k(20).relax(), &t7.relax()); // 2^40-1
        let t9 = FeL::mul(&t8.pow2k(10).relax(), &t6.relax()); // 2^50-1
        let t10 = FeL::mul(&t9.pow2k(50).relax(), &t9.relax()); // 2^100-1
        let t11 = FeL::mul(&t10.pow2k(100).relax(), &t10.relax()); // 2^200-1
        let t12 = FeL::mul(&t11.pow2k(50).relax(), &t9.relax()); // 2^250-1
        FeL::mul(&t12.pow2k(5).relax(), &t3.relax()) // 2^255-21
    }
}

impl FeL {
    #[inline(always)]
    pub fn mul(a: &FeL, b: &FeL) -> Fe {
        let mut o = tight(&[0; 5]);
        fiat_25519_carry_mul(&mut o, &loose(&a.0), &loose(&b.0));
        Fe(o.0)
    }
    #[inline(always)]
    pub fn sq(&self) -> Fe {
        let mut o = tight(&[0; 5]);
        fiat_25519_carry_square(&mut o, &loose(&self.0));
        Fe(o.0)
    }
    #[inline(always)]
    pub fn carry(&self) -> Fe {
        let mut o = tight(&[0; 5]);
        fiat_25519_carry(&mut o, &loose(&self.0));
        Fe(o.0)
    }
}

// ---------------------------------------------------------------------------
// Points — a=-1 twisted Edwards, formulas structurally identical to dalek's.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Extended {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

/// Affine "Niels" precomputed form: (y+x, y-x, 2d·x·y).
#[derive(Clone, Copy)]
pub struct Niels {
    y_plus_x: Fe,
    y_minus_x: Fe,
    xy2d: Fe,
}

const ID_EXTENDED: Extended = Extended {
    x: Fe::ZERO,
    y: Fe::ONE,
    z: Fe::ONE,
    t: Fe::ZERO,
};
const ID_NIELS: Niels = Niels {
    y_plus_x: Fe::ONE,
    y_minus_x: Fe::ONE,
    xy2d: Fe::ZERO,
};

impl Niels {
    fn from_affine(x: &Fe, y: &Fe, d2: &FeL) -> Niels {
        let xy = FeL::mul(&x.relax(), &y.relax());
        Niels {
            y_plus_x: Fe::add(y, x).carry(),
            y_minus_x: Fe::sub(y, x).carry(),
            xy2d: FeL::mul(&xy.relax(), d2),
        }
    }
    /// -P = (-x, y): swap the sum/difference, negate xy2d.
    fn neg(&self) -> Niels {
        Niels {
            y_plus_x: self.y_minus_x,
            y_minus_x: self.y_plus_x,
            xy2d: Fe::neg(&self.xy2d),
        }
    }
}

/// Unified mixed addition (extended + affine-Niels), add-2008-hwcd-3.
/// Complete for a=-1 twisted Edwards — handles P+P and P+(-P) correctly.
#[inline]
fn mixed_add(p: &Extended, q: &Niels) -> Extended {
    let ypx = Fe::add(&p.y, &p.x);
    let ymx = Fe::sub(&p.y, &p.x);
    let pp = FeL::mul(&ypx, &q.y_plus_x.relax());
    let mm = FeL::mul(&ymx, &q.y_minus_x.relax());
    let tt = FeL::mul(&p.t.relax(), &q.xy2d.relax());
    let z2 = Fe::add(&p.z, &p.z).carry();
    let x3 = Fe::sub(&pp, &mm);
    let y3 = Fe::add(&pp, &mm);
    let z3 = Fe::add(&z2, &tt);
    let t3 = Fe::sub(&z2, &tt);
    Extended {
        x: FeL::mul(&x3, &t3),
        y: FeL::mul(&y3, &z3),
        z: FeL::mul(&z3, &t3),
        t: FeL::mul(&x3, &y3),
    }
}

/// Doubling via the projective route (dbl-2008-hwcd), T recomputed.
fn double(p: &Extended) -> Extended {
    let xx = p.x.sq();
    let yy = p.y.sq();
    let zz2 = {
        let z2 = p.z.sq();
        Fe::add(&z2, &z2).carry()
    };
    let xpy_sq = Fe::add(&p.x, &p.y).carry().sq();
    let yy_plus_xx = Fe::add(&yy, &xx).carry();
    let yy_minus_xx = Fe::sub(&yy, &xx).carry();
    let cx = Fe::sub(&xpy_sq, &yy_plus_xx);
    let cy = yy_plus_xx.relax();
    let cz = yy_minus_xx.relax();
    let ct = Fe::sub(&zz2, &yy_minus_xx);
    Extended {
        x: FeL::mul(&cx, &ct),
        y: FeL::mul(&cy, &cz),
        z: FeL::mul(&cz, &ct),
        t: FeL::mul(&cx, &cy),
    }
}

/// Montgomery batch inversion: out[i] = zs[i]^-1, 1 inversion + 3(n-1) muls.
/// All zs must be nonzero (Z of a valid curve point never is).
fn batch_invert(zs: &[Fe], out: &mut [Fe], scratch: &mut [Fe]) {
    let n = zs.len();
    scratch[0] = zs[0];
    for i in 1..n {
        scratch[i] = FeL::mul(&scratch[i - 1].relax(), &zs[i].relax());
    }
    let mut acc = scratch[n - 1].invert();
    for i in (1..n).rev() {
        out[i] = FeL::mul(&scratch[i - 1].relax(), &acc.relax());
        acc = FeL::mul(&acc.relax(), &zs[i].relax());
    }
    out[0] = acc;
}

// ---------------------------------------------------------------------------
// Precomputed comb table.
// ---------------------------------------------------------------------------

/// ed25519 basepoint affine x, y and curve constant d — canonical LE bytes.
/// (Standard RFC-8032 constants; any typo is caught by `self_test`.)
const BX_BYTES: [u8; 32] = [
    0x1a, 0xd5, 0x25, 0x8f, 0x60, 0x2d, 0x56, 0xc9, 0xb2, 0xa7, 0x25, 0x95,
    0x60, 0xc7, 0x2c, 0x69, 0x5c, 0xdc, 0xd6, 0xfd, 0x31, 0xe2, 0xa4, 0xc0,
    0xfe, 0x53, 0x6e, 0xcd, 0xd3, 0x36, 0x69, 0x21,
];
const BY_BYTES: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];
const D_BYTES: [u8; 32] = [
    0xa3, 0x78, 0x59, 0x13, 0xca, 0x4d, 0xeb, 0x75, 0xab, 0xd8, 0x41, 0x41,
    0x4d, 0x0a, 0x70, 0x00, 0x98, 0xe8, 0x79, 0x77, 0x79, 0x40, 0xc7, 0x8c,
    0x73, 0xfe, 0x6f, 0x2b, 0xee, 0x6c, 0x03, 0x52,
];

/// 32 windows × 257 slots (signed digit d+128, d ∈ [-128,128]; slot 128 = 0
/// unused). Entry (w, d) = d·2^{8w}·B in affine-Niels form. ~1 MB, built in
/// a few ms at startup, shared read-only across workers.
pub struct Precomp {
    table: Vec<Niels>,
}

impl Precomp {
    pub fn new() -> Precomp {
        let bx = Fe::from_bytes(&BX_BYTES);
        let by = Fe::from_bytes(&BY_BYTES);
        let d = Fe::from_bytes(&D_BYTES);
        let d2 = Fe::add(&d, &d).carry().relax();

        let mut table = vec![ID_NIELS; 32 * 257];
        let mut base = Extended {
            x: bx,
            y: by,
            z: Fe::ONE,
            t: FeL::mul(&bx.relax(), &by.relax()),
        };
        let mut mult = [ID_EXTENDED; 128];
        let mut zs = [Fe::ZERO; 128];
        let mut inv = [Fe::ZERO; 128];
        let mut scratch = [Fe::ZERO; 128];

        for row in 0..32 {
            // Affine form of this row's base (2^{8·row}·B).
            let zi = base.z.invert();
            let ax = FeL::mul(&base.x.relax(), &zi.relax());
            let ay = FeL::mul(&base.y.relax(), &zi.relax());
            let bn = Niels::from_affine(&ax, &ay, &d2);
            mult[0] = Extended {
                x: ax,
                y: ay,
                z: Fe::ONE,
                t: FeL::mul(&ax.relax(), &ay.relax()),
            };
            for k in 1..128 {
                mult[k] = mixed_add(&mult[k - 1], &bn);
            }
            for k in 0..128 {
                zs[k] = mult[k].z;
            }
            batch_invert(&zs, &mut inv, &mut scratch);
            for k in 0..128 {
                let px = FeL::mul(&mult[k].x.relax(), &inv[k].relax());
                let py = FeL::mul(&mult[k].y.relax(), &inv[k].relax());
                let np = Niels::from_affine(&px, &py, &d2);
                let dv = k + 1; // this entry is (k+1)·base
                table[row * 257 + (128 + dv)] = np;
                table[row * 257 + (128 - dv)] = np.neg();
            }
            // Next row base: 2^8·(this base) = 2·(128·base).
            base = double(&mult[127]);
        }
        Precomp { table }
    }

    #[inline(always)]
    fn entry(&self, window: usize, digit: i16) -> &Niels {
        &self.table[window * 257 + (digit as i32 + 128) as usize]
    }
}

/// Recode 32 bytes (clamped scalar, little-endian) into signed digits
/// d_i ∈ [-128,127], top digit ∈ [64,128] (clamping guarantees the range).
#[inline]
fn recode_signed(a: &[u8; 32]) -> [i16; 32] {
    let mut d = [0i16; 32];
    let mut carry = 0i16;
    for i in 0..31 {
        let v = a[i] as i16 + carry;
        if v >= 128 {
            d[i] = v - 256;
            carry = 1;
        } else {
            d[i] = v;
            carry = 0;
        }
    }
    d[31] = a[31] as i16 + carry;
    d
}

/// Derive BATCH ed25519 pubkeys (RFC-8032: SHA-512 → clamp → a·B → compress)
/// from BATCH seeds, sharing one field inversion across the whole batch.
pub fn derive_pubkeys(
    pre: &Precomp,
    seeds: &[[u8; 32]; BATCH],
    out: &mut [[u8; 32]; BATCH],
) {
    let mut xs = [Fe::ZERO; BATCH];
    let mut ys = [Fe::ZERO; BATCH];
    let mut zs = [Fe::ZERO; BATCH];
    for j in 0..BATCH {
        let h = Sha512::digest(&seeds[j]);
        let mut a = [0u8; 32];
        a.copy_from_slice(&h[..32]);
        a[0] &= 248;
        a[31] &= 127;
        a[31] |= 64;
        let digits = recode_signed(&a);
        let mut acc = ID_EXTENDED;
        for (w, &dg) in digits.iter().enumerate() {
            if dg != 0 {
                acc = mixed_add(&acc, pre.entry(w, dg));
            }
        }
        xs[j] = acc.x;
        ys[j] = acc.y;
        zs[j] = acc.z;
    }
    let mut inv = [Fe::ZERO; BATCH];
    let mut scratch = [Fe::ZERO; BATCH];
    batch_invert(&zs, &mut inv, &mut scratch);
    for j in 0..BATCH {
        let zi = inv[j].relax();
        let ax = FeL::mul(&xs[j].relax(), &zi);
        let ay = FeL::mul(&ys[j].relax(), &zi);
        let mut pk = ay.to_bytes();
        pk[31] |= (ax.to_bytes()[0] & 1) << 7;
        out[j] = pk;
    }
}

// ---------------------------------------------------------------------------
// Suffix matching.
// ---------------------------------------------------------------------------

#[inline(always)]
fn fold(c: u8) -> u8 {
    c.to_ascii_lowercase()
}

/// Validate that every suffix char can appear at all (exact mode: must be a
/// base58 char; any-case mode: some base58 char must case-fold to it).
pub fn validate_suffix(suffix: &[u8], any_case: bool) -> Result<(), String> {
    if suffix.is_empty() {
        return Err("suffix must be non-empty".into());
    }
    for &c in suffix {
        let ok = if any_case {
            B58.iter().any(|&a| fold(a) == fold(c))
        } else {
            B58.contains(&c)
        };
        if !ok {
            return Err(format!(
                "'{}' can never appear in a base58 address{}",
                c as char,
                if any_case { "" } else { " (base58 excludes 0, O, I, l)" }
            ));
        }
    }
    Ok(())
}

/// "base58(pk) ends with suffix" ⇔ pk-as-big-endian-int mod 58^k is in a
/// precomputed target set. One Horner pass, no divmod sweeps, no allocs.
pub struct SuffixFilter {
    modulus: u64,
    targets: Vec<u64>,
}

impl SuffixFilter {
    pub fn new(suffix: &[u8], any_case: bool) -> Result<SuffixFilter, String> {
        let k = suffix.len();
        if k == 0 {
            return Err("suffix must be non-empty".into());
        }
        if k > 8 {
            return Err("fast path supports suffixes up to 8 chars".into());
        }
        let mut targets: Vec<u64> = vec![0];
        let mut scale: u64 = 1; // 58^j
        for j in 0..k {
            let want = suffix[k - 1 - j]; // digit j counts from the END
            let mut opts: Vec<u64> = Vec::new();
            for (idx, &c) in B58.iter().enumerate() {
                let m = if any_case {
                    fold(c) == fold(want)
                } else {
                    c == want
                };
                if m {
                    opts.push(idx as u64);
                }
            }
            if opts.is_empty() {
                return Err(format!(
                    "'{}' can never appear in a base58 address",
                    want as char
                ));
            }
            let mut next = Vec::with_capacity(targets.len() * opts.len());
            for &t in &targets {
                for &o in &opts {
                    next.push(t + o * scale);
                }
            }
            targets = next;
            scale *= 58;
        }
        targets.sort_unstable();
        Ok(SuffixFilter {
            modulus: scale,
            targets,
        })
    }

    #[inline]
    pub fn matches(&self, pk: &[u8; 32]) -> bool {
        let m = self.modulus;
        let mut r: u64 = 0;
        for &b in pk.iter() {
            r = ((r << 8) | b as u64) % m;
        }
        self.targets.binary_search(&r).is_ok()
    }
}

/// Reference matcher (any suffix length): full divmod per trailing digit.
/// Used by the >8-char fallback path and to cross-check SuffixFilter.
pub fn reference_matches(pk: &[u8; 32], suffix: &[u8], any_case: bool) -> bool {
    let mut n = *pk;
    for &want in suffix.iter().rev() {
        let mut rem: u32 = 0;
        for b in n.iter_mut() {
            let cur = (rem << 8) | (*b as u32);
            *b = (cur / 58) as u8;
            rem = cur % 58;
        }
        let got = B58[rem as usize];
        let ok = if any_case {
            fold(got) == fold(want)
        } else {
            got == want
        };
        if !ok {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Startup self-test — refuses to grind if the engine disagrees with the
// reference derivation on a single seed, or the filter with the reference
// matcher on a single case.
// ---------------------------------------------------------------------------

pub fn self_test(pre: &Precomp, oracle: fn(&[u8; 32]) -> [u8; 32]) {
    // 1) Key derivation: 2×BATCH seeds (one deterministic run for
    //    reproducibility, one OS-random run for coverage) vs the oracle.
    let mut seeds = [[0u8; 32]; BATCH];
    let mut pks = [[0u8; 32]; BATCH];
    let mut det = ChaCha8Rng::from_seed([42u8; 32]);
    let mut os = [0u8; 32];
    getrandom::getrandom(&mut os).expect("os rng");
    let mut rnd = ChaCha8Rng::from_seed(os);
    for round in 0..2 {
        let rng: &mut ChaCha8Rng = if round == 0 { &mut det } else { &mut rnd };
        rng.fill_bytes(seeds.as_flattened_mut());
        derive_pubkeys(pre, &seeds, &mut pks);
        for j in 0..BATCH {
            let want = oracle(&seeds[j]);
            assert_eq!(
                pks[j], want,
                "engine self-test FAILED for seed {:02x?} — refusing to grind",
                seeds[j]
            );
        }
    }

    // 2) Suffix filter vs reference matcher on REAL addresses: exact match,
    //    case-flipped match under --any-case, and a mutated-char negative.
    for j in 0..24 {
        let pk = pks[j];
        let s = bs58::encode(&pk).into_string().into_bytes();
        for k in 1..=6usize {
            let sfx = &s[s.len() - k..];
            let f = SuffixFilter::new(sfx, false).expect("filter build");
            assert!(
                f.matches(&pk) && reference_matches(&pk, sfx, false),
                "exact filter self-test failed for suffix {:?}",
                String::from_utf8_lossy(sfx)
            );
            let flipped: Vec<u8> = sfx
                .iter()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect();
            let ff = SuffixFilter::new(&flipped, true).expect("fold filter build");
            assert!(
                ff.matches(&pk) && reference_matches(&pk, &flipped, true),
                "any-case filter self-test failed for suffix {:?}",
                String::from_utf8_lossy(&flipped)
            );
            let mut bad = sfx.to_vec();
            let pos = B58.iter().position(|&c| c == bad[k - 1]).unwrap();
            bad[k - 1] = B58[(pos + 1) % 58];
            let fb = SuffixFilter::new(&bad, false).expect("filter build");
            assert_eq!(
                fb.matches(&pk),
                reference_matches(&pk, &bad, false),
                "negative filter self-test failed"
            );
        }
    }
    eprintln!(
        "engine self-test OK ({} seeds vs reference derivation)",
        2 * BATCH
    );
}
