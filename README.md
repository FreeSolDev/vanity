# Vanity Keypair Generator Service

HTTP API around a **custom, optimized Solana vanity grinder**. Generates Solana keypairs whose public key ends with a chosen base58 suffix. Supports synchronous generation (short suffixes) and async queued jobs (longer suffixes) with on-disk job persistence.

> ⚠️ **This service emits PRIVATE KEYS.** Run it on a private network only — never expose it to the public internet. The reference deployment runs as an **internal-only** container (no public domain), reachable only by trusted services on the same private network.

## What's inside

The binary is **not** the stock `solana-vanity` crate. The Docker image builds a custom grinder (`grinder/`) that is a drop-in for the same CLI/stdout but is materially faster on the same CPU:

- **Seed-correct** — grinds real 32-byte ed25519 seeds and derives the pubkey per RFC-8032 (SHA-512 → clamp → `a·B` → compress), so output is a standard Solana `[seed‖pubkey]` keypair. (The profanity-style scalar-increment trick is deliberately **not** used — it produces keys with no valid seed and is incompatible with Solana's seed-based keypair format.)
- **Fixed-base scalar mult** via a precomputed `ED25519_BASEPOINT_TABLE` (vs the generic mult in `Keypair::new()`).
- **One ChaCha8 CSPRNG seeded per worker** instead of an OS-RNG syscall per attempt.
- **Trailing-base58-digit early reject** — the suffix check is `k` cheap divmod-by-58 steps on the 256-bit pubkey, rejecting ~57/58 of attempts with no full base58 encode/alloc.
- `rayon`-parallel across cores; `--threads` is cgroup-aware by default.

**Correctness is double-gated:** the binary self-verifies every emitted key via `ed25519-dalek` (the reference lib) and aborts on mismatch; `server.js` independently re-derives via `Keypair.fromSeed(seed)` and rejects on any pubkey mismatch. A wrong key cannot ship.

CLI: `solana-vanity --suffix <s> [--threads N]` → stdout `Address: <bs58 pubkey>`, `Private Key (Base58): <bs58 of the 32-byte seed>`, `Time elapsed: <s>`.

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
- **CPU-cap it.** Vanity grinding is CPU-bound; without a hard limit a long grind will starve co-located services. Cap it to a subset of cores (the reference deploy uses 2 of 4).
- **Healthcheck is node-based on purpose.** The runtime image is `node:24-slim` (no `curl`/`wget`). The Dockerfile `HEALTHCHECK` uses `node -e "fetch(...)"`. If your platform enforces the image healthcheck, this is why — don't reintroduce a `curl`-based one.
- The legacy `railway.json` is unused (kept only for history); this is not deployed on Railway.

### Environment variables

| var | default | meaning |
|---|---|---|
| `PORT` | `3000` | HTTP port |
| `MAX_SUFFIX_LENGTH` | `5` | reject suffixes longer than this |
| `TIMEOUT_MS` | `120000` | default per-generation timeout |
| `MAX_CONCURRENT` | `1` | grinds running at once (CPU-bound — keep low) |
| `MAX_QUEUE_DEPTH` | `20` | queued jobs before `429` |
| `JOBS_DIR` | `/data/jobs` | async job persistence dir |

## Performance

Measured on **2 dedicated AMD vCPU** with the optimized grinder. Vanity search is probabilistic — these are single-sample, expect **±3–5× swing** per run; treat them as orders of magnitude, not guarantees:

| suffix | typical | mode |
|---|---|---|
| 1-char | ~10 ms | sync `/generate` |
| 2-char | <200 ms | sync `/generate` |
| 3-char | ~2 s | sync `/generate` |
| 4-char | ~10–30 s | **async `/jobs`** (don't block on sync) |
| 5-char | ~10 min+ | async, expect minutes |
| 6+ char | impractical on CPU | use a GPU grinder instead |

Roughly **3–6× faster keys/sec than the stock `solana-vanity` crate** on the same cores (1–2 char are pure request overhead and don't reflect the speedup). Each added char is ~58× the work — there is no algorithmic shortcut for Solana's seed-based keys; beyond ~5 char you need more cores or a GPU.

## API

### Generate — Synchronous (best for 1–3 char)

Blocks until done.

```bash
POST /generate
Content-Type: application/json

{ "suffix": "vin", "count": 3, "timeout": 60000 }   # count 1-10, timeout ≤ 300000, both optional
```

```json
{
  "success": true,
  "count": 3,
  "suffix": "vin",
  "totalTimeMs": 8500,
  "keypairs": [
    { "publicKey": "...vin", "secretKey": "base58-64-byte-key", "generationTimeMs": 2300, "toolTimeSeconds": 2.1 }
  ]
}
```

`secretKey` is base58 of the standard 64-byte `[seed‖pubkey]` — load with `Keypair.fromSecretKey(bs58.decode(secretKey))`.

### Submit Job — Async (use for 4+ char)

Returns immediately with a job ID.

```bash
POST /jobs
Content-Type: application/json

{ "suffix": "pump", "count": 1, "timeout": 300000 }
```

```json
{ "jobId": "a1b2c3d4-...", "status": "queued", "queuePosition": 1, "suffix": "pump", "count": 1,
  "message": "Job queued. Poll GET /jobs/a1b2c3d4-... for results." }
```

### Check Job — `GET /jobs/:id`

```json
{ "id": "a1b2c3d4-...", "suffix": "pump", "status": "complete",
  "progress": { "completed": 1, "total": 1 },
  "keypairs": [ { "publicKey": "...pump", "secretKey": "base58-64-byte-key", "generationTimeMs": 87000 } ],
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
```
