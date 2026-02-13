# Vanity Keypair Generator Service

Containerized solana-vanity with HTTP API for cloud deployment. Supports both synchronous generation and async queued jobs with persistent storage.

## Deploy to Railway

1. Create new project on Railway
2. Point to this repo
3. Add a **Volume** (Settings > Volumes) mounted at `/data`
4. Optional environment variables:
   - `MAX_SUFFIX_LENGTH` - Max suffix chars (default: 5)
   - `TIMEOUT_MS` - Generation timeout in ms (default: 120000)
   - `MAX_CONCURRENT` - Max jobs running at once (default: 1)
   - `MAX_QUEUE_DEPTH` - Max queued jobs before rejecting (default: 20)
   - `JOBS_DIR` - Job storage path (default: `/data/jobs`)

## API Usage

### Generate Keypair(s) — Synchronous

Blocks until done. Best for short suffixes (1-3 chars).

```bash
POST /generate
Content-Type: application/json

{
  "suffix": "vin",
  "count": 3,        # optional, 1-10
  "timeout": 60000   # optional, max 300000
}
```

Response:
```json
{
  "success": true,
  "count": 3,
  "suffix": "vin",
  "totalTimeMs": 8500,
  "keypairs": [
    {
      "publicKey": "...vin",
      "secretKey": "base58-64-byte-key",
      "generationTimeMs": 2300,
      "toolTimeSeconds": 2.1
    }
  ]
}
```

### Submit Job — Async

Returns immediately with a job ID. Best for longer suffixes (4+ chars) that take 30s+.

```bash
POST /jobs
Content-Type: application/json

{
  "suffix": "pump",
  "count": 1,
  "timeout": 300000
}
```

Response (`202 Accepted`):
```json
{
  "jobId": "a1b2c3d4-...",
  "status": "queued",
  "queuePosition": 1,
  "suffix": "pump",
  "count": 1,
  "message": "Job queued. Poll GET /jobs/a1b2c3d4-... for results."
}
```

### Check Job Status

```bash
GET /jobs/:id
```

Response (while running):
```json
{
  "id": "a1b2c3d4-...",
  "suffix": "pump",
  "status": "running",
  "progress": { "completed": 0, "total": 1 },
  "keypairs": [],
  "error": null
}
```

Response (when complete):
```json
{
  "id": "a1b2c3d4-...",
  "suffix": "pump",
  "status": "complete",
  "progress": { "completed": 1, "total": 1 },
  "keypairs": [
    {
      "publicKey": "...pump",
      "secretKey": "base58-64-byte-key",
      "generationTimeMs": 87000,
      "toolTimeSeconds": 86.5
    }
  ]
}
```

### List All Jobs

Returns a summary of all jobs (no secret keys exposed).

```bash
GET /jobs
```

### Health Check

```bash
GET /health
```

Returns queue stats alongside status:
```json
{
  "status": "ok",
  "timestamp": 1700000000000,
  "queue": { "running": 1, "queued": 3, "maxConcurrent": 1, "maxDepth": 20 }
}
```

## Queue & Rate Limiting

- Jobs are processed one at a time (`MAX_CONCURRENT=1`) since generation is CPU-bound
- When the queue is full, new submissions are rejected with `429 Too Many Requests`
- Job state is saved to the Railway volume at `/data/jobs/` as JSON files
- Jobs survive restarts and redeploys — incomplete jobs are automatically re-queued on startup

## Local Testing

```bash
# Build
docker build -t vanity-generator .

# Run (mount a local dir as the volume)
docker run -p 3000:3000 -v ./data:/data vanity-generator

# Sync test
curl -X POST http://localhost:3000/generate \
  -H "Content-Type: application/json" \
  -d '{"suffix": "vin"}'

# Async test
curl -X POST http://localhost:3000/jobs \
  -H "Content-Type: application/json" \
  -d '{"suffix": "pump"}'
# then poll with the returned jobId
```

## Performance Notes

- Railway uses CPU only (no GPU)
- 3-char suffix: ~0.5-2s
- 4-char suffix: ~2-10s
- 5-char suffix: ~30s-5min
- 6+ chars: Not recommended for cloud (too slow/expensive)
