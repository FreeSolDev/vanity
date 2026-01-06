/**
 * Vanity Keypair Generator API
 *
 * Simple HTTP API wrapping solana-vanity for cloud deployment.
 *
 * Endpoints:
 *   POST /generate - Generate a vanity keypair
 *   GET /health - Health check
 */

import express from 'express';
import { spawn } from 'child_process';
import { Keypair } from '@solana/web3.js';
import bs58 from 'bs58';

const app = express();
app.use(express.json());

const PORT = process.env.PORT || 3000;
const MAX_SUFFIX_LENGTH = parseInt(process.env.MAX_SUFFIX_LENGTH) || 5;
const TIMEOUT_MS = parseInt(process.env.TIMEOUT_MS) || 120000; // 2 min default

// Generate a single vanity keypair
function generateKeypair(suffix, timeoutMs) {
  return new Promise((resolve, reject) => {
    const startTime = Date.now();
    let killed = false;

    const proc = spawn('solana-vanity', ['--suffix', suffix], {
      stdio: ['ignore', 'pipe', 'pipe']
    });

    const timeout = setTimeout(() => {
      killed = true;
      proc.kill('SIGTERM');
      reject(new Error(`Generation timed out after ${timeoutMs}ms`));
    }, timeoutMs);

    let stdout = '';
    let stderr = '';

    proc.stdout.on('data', (data) => { stdout += data.toString(); });
    proc.stderr.on('data', (data) => { stderr += data.toString(); });

    proc.on('close', (code) => {
      clearTimeout(timeout);
      if (killed) return;

      const output = stdout + stderr;

      if (code !== 0) {
        reject(new Error(`solana-vanity failed: ${output}`));
        return;
      }

      try {
        const addressMatch = output.match(/Address:\s*([A-Za-z0-9]{32,50})/);
        const privateKeyMatch = output.match(/Private Key \(Base58\):\s*([A-Za-z0-9]{32,90})/);
        const timeMatch = output.match(/Time elapsed:\s*([\d.]+)/);

        if (!addressMatch || !privateKeyMatch) {
          reject(new Error(`Could not parse output`));
          return;
        }

        const publicKey = addressMatch[1];
        const secretKeyBase58 = privateKeyMatch[1];
        const toolTime = timeMatch ? parseFloat(timeMatch[1]) : null;

        // Reconstruct full keypair from seed
        const secretKeyBytes = bs58.decode(secretKeyBase58);
        let fullSecretKey;
        if (secretKeyBytes.length === 32) {
          const keypair = Keypair.fromSeed(secretKeyBytes);
          fullSecretKey = keypair.secretKey;
        } else {
          fullSecretKey = secretKeyBytes;
        }

        const keypair = Keypair.fromSecretKey(fullSecretKey);

        if (keypair.publicKey.toBase58() !== publicKey) {
          reject(new Error(`Public key mismatch`));
          return;
        }

        resolve({
          publicKey,
          secretKey: bs58.encode(fullSecretKey),
          generationTimeMs: Date.now() - startTime,
          toolTimeSeconds: toolTime
        });
      } catch (err) {
        reject(new Error(`Failed to parse keypair: ${err.message}`));
      }
    });

    proc.on('error', (err) => {
      clearTimeout(timeout);
      reject(new Error(`Failed to spawn solana-vanity: ${err.message}`));
    });
  });
}

// Health check
app.get('/health', (_req, res) => {
  res.json({ status: 'ok', timestamp: Date.now() });
});

// Generate endpoint
app.post('/generate', async (req, res) => {
  const { suffix, count = 1, timeout = TIMEOUT_MS } = req.body;

  // Validation
  if (!suffix || typeof suffix !== 'string') {
    return res.status(400).json({ error: 'Missing or invalid suffix' });
  }

  if (!/^[a-zA-Z0-9]+$/.test(suffix)) {
    return res.status(400).json({ error: 'Suffix must be alphanumeric' });
  }

  if (suffix.length > MAX_SUFFIX_LENGTH) {
    return res.status(400).json({
      error: `Suffix too long. Max ${MAX_SUFFIX_LENGTH} chars (longer = exponentially slower)`
    });
  }

  const requestedCount = Math.min(Math.max(1, parseInt(count)), 10); // Max 10 per request
  const effectiveTimeout = Math.min(timeout, 300000); // Max 5 min

  console.log(`[${new Date().toISOString()}] Generating ${requestedCount} keypair(s) with suffix "${suffix}"`);

  try {
    const results = [];
    const startTime = Date.now();

    for (let i = 0; i < requestedCount; i++) {
      const keypair = await generateKeypair(suffix, effectiveTimeout);
      results.push(keypair);
    }

    res.json({
      success: true,
      count: results.length,
      suffix,
      totalTimeMs: Date.now() - startTime,
      keypairs: results
    });
  } catch (err) {
    console.error(`[${new Date().toISOString()}] Error:`, err.message);
    res.status(500).json({
      success: false,
      error: err.message
    });
  }
});

// Start server
app.listen(PORT, () => {
  console.log(`Vanity Generator API running on port ${PORT}`);
  console.log(`Max suffix length: ${MAX_SUFFIX_LENGTH}`);
  console.log(`Default timeout: ${TIMEOUT_MS}ms`);
});
