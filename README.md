# Vanity Keypair Generator Service

HTTP API around a **custom, optimized Solana vanity grinder**. Generates Solana keypairs whose public key ends with a chosen base58 suffix. Supports synchronous generation (short suffixes) and async queued jobs (longer suffixes) with on-disk job persistence.

> ⚠️ **This service emits PRIVATE KEYS.** Run it on a private network only — never expose it to the public internet. The reference deployment runs as an **internal-only** container (no public domain), reachable only by trusted services on the same private network.

## What's inside

The binary is **not** the stock `solana-vanity` crate. The Docker image builds a custom grinder (`grinder/`, v0.3) that is a drop-in for the same CLI/stdout but is an order of magnitude faster on the same CPU:

- **Seed-correct** — grinds real 32-byte ed25519 seeds and derives the pubkey per RFC-8032 (SHA-512 → clamp → `a·B` → compress), so output is a standard Solana `[seed‖pubkey]` keypair. (The profanity-style scalar-increment trick is deliberately **not** used — it produces keys with no valid seed and is incompatible with Solana's seed-based keypair format.)
- **Batched vartime comb engine** (`grinder/src/engine.rs`): pubkey = Σ dᵢ·(2⁸ⁱ·B) over a ~1 MB precomputed signed radix-256 table — 32 direct-indexed point additions per key, no doublings, none of dalek's constant-time table-scan overhead (constant time defends keys from timing attacks; candidates ground by the million and thrown away don't need it).
- **Montgomery batch inversion** — point compression needs affine y = Y/Z, a ~254-squaring field inversion. The engine compresses 128 keys per batch with ONE shared inversion (~3 muls/key) instead of one inversion per key.
- **Residue suffix filter** — "base58 ends with suffix" ⇔ pubkey mod 58^k ∈ precomputed target set: one Horner pass over 32 bytes, no divmods per digit, no base58 encode/alloc per attempt.
- **`--any-case`** — case-insensitive matching: each letter matches 2 ways, so a 4-letter suffix is ~16× less work (seconds instead of minutes). `O`/`I`/`l` become grindable (matched as `o`/`i`/`L`); only `0` never appears in base58.
- Field arithmetic is **fiat-crypto** (formally verified); one ChaCha8 CSPRNG per worker; `rayon`-parallel; `--threads` cgroup-aware.

**Correctness is triple-gated:** at every start the engine must reproduce the reference dalek derivation on 256 seeds bit-for-bit and the filter must agree with a reference matcher on real encoded addresses (`self_test`, aborts the process on any mismatch); every emitted key is re-verified via `ed25519-dalek` before printing; and `server.js` independently re-derives via `Keypair.fromSeed(seed)` and rejects on any pubkey mismatch. A wrong key cannot ship.

CLI: `solana-vanity --suffix <s> [--threads N] [--any-case] [--bench N]` → stdout `Address: <bs58 pubkey>`, `Private Key (Base58): <bs58 of the 32-byte seed>`, `Time elapsed: <s>`. `--bench N` derives N keys and prints a `keys/s` rate instead of grinding.

## Deploy (Docker — anywhere)

It's a plain container; deploy on any Docker host.

```bash
docker build -t vanity-generator .
# Internal-only: bind to loopback (DO NOT publish 0.0.0.0)
docker run -d --name vanity \
  -p 127.0.0.1:3000:3000 \
  --cpus 2 \
  -e MAX_SUFFIX_LENGTH=4 \
  vanity-generator
```

**Coolify / orchestrated deploy notes:**
- **No public domain.** It mints keypairs — keep it on the internal network only; reach it via the orchestrator's internal DNS (e.g. a stable network alias `http://vanity:3000`), not a public FQDN.
- **CPU-cap it.** Vanity grinding is CPU-bound; without a hard limit a long grind will starve co-located services. Cap it to a subset of cores (the reference deploy uses 2 of 4). More vCPUs = proportionally faster grinds — the engine scales linearly.
- **Healthcheck is node-based on purpose.** The runtime image is `node:24-slim` (no `curl`/`wget`). The Dockerfile `HEALTHCHECK` uses `node -e "fetch(...)"`. If your platform enforces the image healthcheck, this is why — don't reintroduce a `curl`-based one.
- The legacy `railway.json` is unused (kept only for history); this is not deployed on Railway.

**Local (no Docker):** `server.js` spawns whatever `solana-vanity` is on `PATH`. After changing `grinder/`, reinstall or local grinds silently keep using the old binary:

```bash
RUSTFLAGS="-C target-cpu=native" cargo install --path grinder --force
```

### Environment variables

| var | default | meaning |
|---|---|---|
| `PORT` | `3000` | HTTP port |
| `MAX_SUFFIX_LENGTH` | `4` | reject suffixes longer than this |
| `TIMEOUT_MS` | `120000` | default per-generation timeout |
| `MAX_CONCURRENT` | `1` | grinds running at once (CPU-bound — keep low) |
| `MAX_QUEUE_DEPTH` | `20` | queued jobs before `429` |
| `JOBS_DIR` | `/data/jobs` | async job persistence dir |

## Performance

Measured keys/sec on the same 6-core/12-thread i7-8700B (12 threads / 1 thread):

| binary | keys/s (12T) | keys/s (1T) | 4-char mean |
|---|---|---|---|
| stock `solana-vanity` crate (v0.1.1) | ~68k | — | ~2.8 min |
| v0.2 grinder (dalek fixed-base + early reject) | 156k | 25k | ~72 s |
| **v0.3 engine (comb + batch inversion)** | **426k** | **93k** | **~27 s** |

Typical wall-clock (12 threads; single-sample, expect ±3-5× swing — a 4-char exact grind is a ~1-in-11.3M lottery per attempt):

| suffix | exact | `caseInsensitive` (letters) |
|---|---|---|
| 3-char | ~0.5 s | < 0.5 s |
| 4-char | ~10-60 s | **~1-3 s** |
| 5-char | ~10-45 min | ~1-3 min |

A 2-vCPU deploy is ~4-6× slower than the 12-thread numbers. Each added char is ~58× the work (~29× case-insensitive) — there is no algorithmic shortcut for Solana's seed-based keys; beyond ~5 chars you need more cores or a GPU.

## API

Full reference with examples: [API.md](./API.md).

### Generate — Synchronous (best for 1-3 char, or any-case 4 char)

Blocks until done.

```bash
POST /generate
Content-Type: application/json

{ "suffix": "vin", "count": 3, "timeout": 60000, "caseInsensitive": false }
```

```json
{
  "success": true,
  "count": 3,
  "suffix": "vin",
  "caseInsensitive": false,
  "totalTimeMs": 8500,
  "keypairs": [
    { "publicKey": "...vin", "secretKey": "base58-64-byte-key", "generationTimeMs": 2300, "toolTimeSeconds": 2.1 }
  ]
}
```

`secretKey` is base58 of the standard 64-byte `[seed‖pubkey]` — load with `Keypair.fromSecretKey(bs58.decode(secretKey))`. With `caseInsensitive: true` the found address may differ from the requested suffix in letter case.

### Submit Job — Async (use for exact 4+ char)

Returns immediately with a job ID.

```bash
POST /jobs
Content-Type: application/json

{ "suffix": "pump", "count": 1, "timeout": 300000, "caseInsensitive": true }
```

```json
{ "jobId": "a1b2c3d4-...", "status": "queued", "queuePosition": 1, "suffix": "pump",
  "caseInsensitive": true, "count": 1,
  "message": "Job queued. Poll GET /jobs/a1b2c3d4-... for results." }
```

### Check Job — `GET /jobs/:id`

```json
{ "id": "a1b2c3d4-...", "suffix": "pump", "status": "complete",
  "progress": { "completed": 1, "total": 1 },
  "keypairs": [ { "publicKey": "...pump", "secretKey": "base58-64-byte-key", "generationTimeMs": 2100 } ],
  "error": null }
```

`status` ∈ `queued | running | complete | failed`.

### List Jobs — `GET /jobs`

Summary of all jobs (no secret keys exposed).

### Health — `GET /health`

```json
{ "status": "ok", "timestamp": 1700000000000,
  "queue": { "running": 1, "queued": 3, "maxConcurrent": 1, "maxDepth": 20 } }
```

## Queue & persistence

- Grinds run one at a time (`MAX_CONCURRENT=1`) — generation is CPU-bound.
- Queue full → new submissions get `429 Too Many Requests`.
- Job state is written to `JOBS_DIR` (`/data/jobs`) as JSON; mount a volume there to survive restarts. Incomplete jobs are re-queued on startup. (If you don't mount a volume, async job *history* is lost on restart — fine for ephemeral use; in-flight grinds just need re-submitting.)

## Local testing

```bash
docker build -t vanity-generator .
docker run --rm -p 127.0.0.1:3000:3000 vanity-generator
curl -s -X POST http://127.0.0.1:3000/generate -H 'Content-Type: application/json' -d '{"suffix":"ab"}'
curl -s -X POST http://127.0.0.1:3000/generate -H 'Content-Type: application/json' -d '{"suffix":"PUMP","caseInsensitive":true}'
```
