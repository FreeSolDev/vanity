# Vanity Generator API Guide

Base URL: `http://<your-host>:3000` (reference deploy is **internal-only** — e.g. `http://vanity:3000` on the private network; the old Railway URL is defunct)

## Quick Start

### Fast generation (1-3 char suffix)

Use the sync endpoint — it blocks until done and returns the keys directly.

```bash
curl -X POST http://vanity:3000/generate \
  -H "Content-Type: application/json" \
  -d '{"suffix": "vin"}'
```

### Longer generation (4+ char suffix)

Use the async jobs endpoint — it returns a job ID instantly, then you poll for results.

```bash
# 1. Submit the job
curl -X POST http://vanity:3000/jobs \
  -H "Content-Type: application/json" \
  -d '{"suffix": "pump"}'

# Response:
# {
#   "jobId": "6c9de77c-7bad-4e5f-8115-59ed449b2274",
#   "status": "queued",
#   "queuePosition": 1,
#   "message": "Job queued. Poll GET /jobs/6c9de77c-... for results."
# }

# 2. Poll for results (use the jobId from above)
curl http://vanity:3000/jobs/6c9de77c-7bad-4e5f-8115-59ed449b2274

# When done:
# {
#   "status": "complete",
#   "keypairs": [{ "publicKey": "...pump", "secretKey": "..." }]
# }
```

---

## Endpoints

### `POST /generate` — Sync Generation

Blocks until all keypairs are generated. Best for short suffixes that take a few seconds.

**Request:**
```json
{
  "suffix": "vin",
  "count": 3,
  "timeout": 60000,
  "caseInsensitive": false
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `suffix` | Yes | — | Alphanumeric string, max 4 chars (env `MAX_SUFFIX_LENGTH`) |
| `count` | No | 1 | Number of keypairs to generate (1-10) |
| `timeout` | No | 120000 | Timeout per keypair in ms (max 300000) |
| `caseInsensitive` | No | false | Match the suffix in any letter case — see below |

**`caseInsensitive: true` is the fast mode.** Each letter can match 2 ways (`p` or `P`), so a 4-letter suffix needs ~16× fewer attempts — typically seconds instead of minutes. The found address may differ in case from what you asked for (request `PUMP`, get `...RoPUmp`). Bonus: `O`, `I` and `l` — normally impossible in base58 — are accepted and matched as `o`, `i`, `L`. Only `0` can never appear. The response echoes `caseInsensitive` so you know which mode produced it.

**Response (`200`):**
```json
{
  "success": true,
  "count": 3,
  "suffix": "vin",
  "caseInsensitive": false,
  "totalTimeMs": 8500,
  "keypairs": [
    {
      "publicKey": "3RdpGUTfnrasHSGYAR3y7A1QeYspcAgj2Hb6MPydvin",
      "secretKey": "base58-encoded-64-byte-secret-key",
      "generationTimeMs": 2300,
      "toolTimeSeconds": 2.1
    }
  ]
}
```

---

### `POST /jobs` — Submit Async Job

Returns immediately with a job ID. The job enters a queue and runs in the background. Best for 4+ char suffixes that can take 30s-5min.

**Request:**
```json
{
  "suffix": "pump",
  "count": 1,
  "timeout": 300000,
  "caseInsensitive": true
}
```

Same fields as `/generate`.

**Response (`202 Accepted`):**
```json
{
  "jobId": "6c9de77c-7bad-4e5f-8115-59ed449b2274",
  "status": "queued",
  "queuePosition": 1,
  "suffix": "pump",
  "caseInsensitive": true,
  "count": 1,
  "message": "Job queued. Poll GET /jobs/6c9de77c-... for results."
}
```

**Rate limit:** If the queue is full (default: 20 jobs), you'll get a `429`:
```json
{
  "error": "Queue is full (20 jobs). Try again later.",
  "queueDepth": 20
}
```

---

### `GET /jobs/:id` — Check Job Status

Poll this endpoint to track progress and get results.

**Job statuses:**

| Status | Meaning |
|--------|---------|
| `queued` | Waiting in line, not started yet |
| `running` | Currently generating keypairs |
| `complete` | Done — keypairs are in the response |
| `failed` | Something went wrong — check `error` field |

**Response (queued):**
```json
{
  "id": "6c9de77c-...",
  "status": "queued",
  "queuePosition": 3,
  "progress": { "completed": 0, "total": 1 }
}
```

**Response (running, multi-keypair job):**
```json
{
  "id": "6c9de77c-...",
  "status": "running",
  "queuePosition": null,
  "progress": { "completed": 2, "total": 5 },
  "keypairs": [
    { "publicKey": "...pump", "secretKey": "...", "generationTimeMs": 45000 },
    { "publicKey": "...pump", "secretKey": "...", "generationTimeMs": 62000 }
  ]
}
```

**Response (complete):**
```json
{
  "id": "6c9de77c-...",
  "status": "complete",
  "progress": { "completed": 1, "total": 1 },
  "keypairs": [
    {
      "publicKey": "3tuvbo4r6xxqPwLSgJaYCT6VhBS2iScV7s12TYQhpump",
      "secretKey": "3UUH7EbfRk29qEhndtehVzKtCmkL97hv8PLxo6qzqBRg...",
      "generationTimeMs": 5936,
      "toolTimeSeconds": 5.887
    }
  ]
}
```

**Response (failed):**
```json
{
  "id": "6c9de77c-...",
  "status": "failed",
  "error": "Generation timed out after 120000ms"
}
```

---

### `GET /jobs` — List All Jobs

Returns a summary of all jobs. Secret keys are **not** included in this view.

**Response:**
```json
{
  "total": 4,
  "jobs": [
    {
      "id": "6c9de77c-...",
      "suffix": "pump",
      "status": "complete",
      "progress": { "completed": 1, "total": 1 },
      "createdAt": "2026-02-13T01:46:49.661Z",
      "completedAt": "2026-02-13T01:46:55.601Z"
    },
    {
      "id": "74a50658-...",
      "suffix": "bonk",
      "status": "running",
      "progress": { "completed": 0, "total": 1 },
      "queuePosition": null,
      "createdAt": "2026-02-13T01:52:31.459Z",
      "completedAt": null
    }
  ]
}
```

---

### `GET /health` — Health Check

**Response:**
```json
{
  "status": "ok",
  "timestamp": 1700000000000,
  "queue": {
    "running": 1,
    "queued": 3,
    "maxConcurrent": 1,
    "maxDepth": 20
  }
}
```

---

## Polling Example (JavaScript)

```javascript
async function generateVanity(suffix) {
  // Submit job
  const { jobId } = await fetch('http://vanity:3000/jobs', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ suffix })
  }).then(r => r.json());

  // Poll until done
  while (true) {
    const job = await fetch(`http://vanity:3000/jobs/${jobId}`)
      .then(r => r.json());

    if (job.status === 'complete') return job.keypairs;
    if (job.status === 'failed') throw new Error(job.error);

    // Wait 5s between polls
    await new Promise(r => setTimeout(r, 5000));
  }
}

const keys = await generateVanity('pump');
console.log(keys[0].publicKey); // "...pump"
```

---

## Expected Generation Times

Measured with the v0.3 batched grinder on a 6-core/12-thread desktop (~425k keys/s). A small 2-vCPU deploy is roughly 4-6× slower; vanity search is probabilistic, so expect ±3-5× swing per run.

| Suffix Length | Example | Exact case | `caseInsensitive` (letters) | Recommended Endpoint |
|---------------|---------|-----------|------------------------------|---------------------|
| 1-2 chars | `ab` | < 1s | < 1s | `POST /generate` |
| 3 chars | `vin` | ~0.5s | < 0.5s | `POST /generate` |
| 4 chars | `pump` | ~10-60s | **~1-3s** | `POST /jobs` (exact) / either (CI) |
| 5 chars | `money` | ~10-45min | ~1-3min | `POST /jobs` |
| 6+ chars | — | days+ | hours | Not recommended on CPU |
