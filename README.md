# Vanity Keypair Generator Service

Containerized solana-vanity with HTTP API for cloud deployment.

## Deploy to Railway

1. Create new project on Railway
2. Point to this directory (`docker/vanity-generator`)
3. Optional environment variables:
   - `MAX_SUFFIX_LENGTH` - Max suffix chars (default: 5)
   - `TIMEOUT_MS` - Generation timeout (default: 120000)

## API Usage

### Generate Keypair(s)

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

### Health Check

```bash
GET /health
```

## Local Testing

```bash
# Build
docker build -t vanity-generator .

# Run
docker run -p 3000:3000 vanity-generator

# Test
curl -X POST http://localhost:3000/generate \
  -H "Content-Type: application/json" \
  -d '{"suffix": "vin"}'
```

## Performance Notes

- Railway uses CPU only (no GPU)
- 3-char suffix: ~0.5-2s
- 4-char suffix: ~2-10s
- 5-char suffix: ~30s-5min
- 6+ chars: Not recommended for cloud (too slow/expensive)
