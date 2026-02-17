/**
 * Vanity Keypair Generator API
 *
 * HTTP API wrapping solana-vanity for cloud deployment.
 *
 * Endpoints:
 *   POST /generate      - Generate keypair(s) synchronously (original behavior)
 *   POST /jobs          - Submit an async generation job (queued)
 *   GET  /jobs          - List all jobs (summary, no secret keys)
 *   GET  /jobs/:id      - Check job status / retrieve results
 *   GET  /health        - Health check
 */

import express from 'express';
import { spawn } from 'child_process';
import { Keypair } from '@solana/web3.js';
import bs58 from 'bs58';
import { randomUUID } from 'crypto';
import { readFile, writeFile, readdir, mkdir } from 'fs/promises';
import { join } from 'path';

const app = express();
app.use(express.json());

const PORT = process.env.PORT || 3000;
const MAX_SUFFIX_LENGTH = 4;
const TIMEOUT_MS = parseInt(process.env.TIMEOUT_MS) || 120000;
const JOBS_DIR = process.env.JOBS_DIR || '/data/jobs';
const MAX_CONCURRENT = parseInt(process.env.MAX_CONCURRENT) || 1;
const MAX_QUEUE_DEPTH = parseInt(process.env.MAX_QUEUE_DEPTH) || 20;

// Ensure jobs directory exists on startup
await mkdir(JOBS_DIR, { recursive: true });

// --- Job persistence helpers ---

function jobPath(id) {
  return join(JOBS_DIR, `${id}.json`);
}

async function saveJob(job) {
  job.updatedAt = new Date().toISOString();
  await writeFile(jobPath(job.id), JSON.stringify(job, null, 2));
}

async function loadJob(id) {
  try {
    const data = await readFile(jobPath(id), 'utf-8');
    return JSON.parse(data);
  } catch {
    return null;
  }
}

// --- Queue ---

const queue = [];    // job IDs waiting to run
let running = 0;     // how many jobs are currently running

function queuePosition(jobId) {
  const idx = queue.indexOf(jobId);
  return idx === -1 ? null : idx + 1;
}

async function enqueue(jobId) {
  queue.push(jobId);
  drain(); // try to start it immediately if there's capacity
}

async function drain() {
  while (running < MAX_CONCURRENT && queue.length > 0) {
    const jobId = queue.shift();
    running++;
    // Update queue positions for all remaining queued jobs
    for (const queuedId of queue) {
      const queuedJob = await loadJob(queuedId);
      if (queuedJob) {
        queuedJob.queuePosition = queuePosition(queuedId);
        await saveJob(queuedJob);
      }
    }
    runJob(jobId).finally(() => {
      running--;
      drain();
    });
  }
}

// --- Keypair generation ---

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

// --- Background job runner ---

async function runJob(jobId) {
  const job = await loadJob(jobId);
  if (!job) return;

  job.status = 'running';
  job.queuePosition = null;
  job.startedAt = new Date().toISOString();
  await saveJob(job);

  console.log(`[${new Date().toISOString()}] Job ${jobId}: generating ${job.count} keypair(s) with suffix "${job.suffix}"`);

  try {
    for (let i = 0; i < job.count; i++) {
      const keypair = await generateKeypair(job.suffix, job.timeout);
      job.keypairs.push(keypair);
      job.progress.completed = i + 1;
      await saveJob(job);
    }

    job.status = 'complete';
    job.completedAt = new Date().toISOString();
    await saveJob(job);

    console.log(`[${new Date().toISOString()}] Job ${jobId}: complete (${job.keypairs.length} keypairs)`);
  } catch (err) {
    job.status = 'failed';
    job.error = err.message;
    await saveJob(job);

    console.error(`[${new Date().toISOString()}] Job ${jobId}: failed - ${err.message}`);
  }
}

// --- Startup: recover incomplete jobs from disk ---

async function recoverJobs() {
  try {
    const files = await readdir(JOBS_DIR);
    const toRecover = [];

    for (const file of files) {
      if (!file.endsWith('.json')) continue;
      try {
        const data = await readFile(join(JOBS_DIR, file), 'utf-8');
        const job = JSON.parse(data);
        if (job.status === 'queued' || job.status === 'running') {
          // Reset running jobs back to queued so they start fresh
          job.status = 'queued';
          job.keypairs = [];
          job.progress.completed = 0;
          await saveJob(job);
          toRecover.push(job);
        }
      } catch {
        // skip malformed files
      }
    }

    // Sort by creation time so oldest jobs run first
    toRecover.sort((a, b) => new Date(a.createdAt) - new Date(b.createdAt));
    for (const job of toRecover) {
      console.log(`[${new Date().toISOString()}] Recovering job ${job.id} (suffix: "${job.suffix}")`);
      await enqueue(job.id);
    }

    if (toRecover.length > 0) {
      console.log(`[${new Date().toISOString()}] Recovered ${toRecover.length} job(s) from disk`);
    }
  } catch {
    // No jobs dir yet, that's fine
  }
}

await recoverJobs();

// --- Shared validation ---

function validateSuffix(suffix) {
  if (!suffix || typeof suffix !== 'string') {
    return 'Missing or invalid suffix';
  }
  if (!/^[a-zA-Z0-9]+$/.test(suffix)) {
    return 'Suffix must be alphanumeric';
  }
  // Base58 excludes 0, O, I, l — Solana addresses can never contain these
  const invalid = suffix.match(/[0OIl]/g);
  if (invalid) {
    return `Suffix contains invalid base58 character(s): ${[...new Set(invalid)].join(', ')}. Solana addresses cannot contain 0, O, I, or l.`;
  }
  if (suffix.length > MAX_SUFFIX_LENGTH) {
    return `Suffix too long. Max ${MAX_SUFFIX_LENGTH} chars (longer = exponentially slower)`;
  }
  return null;
}

// --- Routes ---

// Health check
app.get('/health', (_req, res) => {
  res.json({
    status: 'ok',
    timestamp: Date.now(),
    queue: { running, queued: queue.length, maxConcurrent: MAX_CONCURRENT, maxDepth: MAX_QUEUE_DEPTH }
  });
});

// Synchronous generate (original behavior)
app.post('/generate', async (req, res) => {
  const { suffix, count = 1, timeout = TIMEOUT_MS } = req.body;

  const error = validateSuffix(suffix);
  if (error) return res.status(400).json({ error });

  const requestedCount = Math.min(Math.max(1, parseInt(count)), 10);
  const effectiveTimeout = Math.min(timeout, 300000);

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

// Submit an async job
app.post('/jobs', async (req, res) => {
  const { suffix, count = 1, timeout = TIMEOUT_MS } = req.body;

  const error = validateSuffix(suffix);
  if (error) return res.status(400).json({ error });

  // Rate limit: reject if queue is full
  if (queue.length >= MAX_QUEUE_DEPTH) {
    return res.status(429).json({
      error: `Queue is full (${MAX_QUEUE_DEPTH} jobs). Try again later.`,
      queueDepth: queue.length
    });
  }

  const requestedCount = Math.min(Math.max(1, parseInt(count)), 10);
  const effectiveTimeout = Math.min(timeout, 300000);

  const job = {
    id: randomUUID(),
    suffix,
    count: requestedCount,
    timeout: effectiveTimeout,
    status: 'queued',
    queuePosition: queue.length + 1,
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    startedAt: null,
    completedAt: null,
    progress: { completed: 0, total: requestedCount },
    keypairs: [],
    error: null
  };

  await saveJob(job);
  await enqueue(job.id);

  res.status(202).json({
    jobId: job.id,
    status: job.status,
    queuePosition: queuePosition(job.id),
    suffix: job.suffix,
    count: job.count,
    message: `Job queued. Poll GET /jobs/${job.id} for results.`
  });
});

// Check job status
app.get('/jobs/:id', async (req, res) => {
  const job = await loadJob(req.params.id);

  if (!job) {
    return res.status(404).json({ error: 'Job not found' });
  }

  // Update live queue position if still queued
  if (job.status === 'queued') {
    job.queuePosition = queuePosition(job.id);
  }

  res.json(job);
});

// List all jobs (summary view, no secret keys)
app.get('/jobs', async (_req, res) => {
  try {
    const files = await readdir(JOBS_DIR);
    const jobs = [];

    for (const file of files) {
      if (!file.endsWith('.json')) continue;
      try {
        const data = await readFile(join(JOBS_DIR, file), 'utf-8');
        const job = JSON.parse(data);
        jobs.push({
          id: job.id,
          suffix: job.suffix,
          status: job.status,
          progress: job.progress,
          queuePosition: job.status === 'queued' ? queuePosition(job.id) : null,
          createdAt: job.createdAt,
          completedAt: job.completedAt
        });
      } catch {
        // skip malformed files
      }
    }

    jobs.sort((a, b) => new Date(b.createdAt) - new Date(a.createdAt));
    res.json({ jobs, total: jobs.length });
  } catch {
    res.json({ jobs: [], total: 0 });
  }
});

// Start server
app.listen(PORT, () => {
  console.log(`Vanity Generator API running on port ${PORT}`);
  console.log(`Jobs directory: ${JOBS_DIR}`);
  console.log(`Max suffix length: ${MAX_SUFFIX_LENGTH}`);
  console.log(`Default timeout: ${TIMEOUT_MS}ms`);
  console.log(`Queue: max ${MAX_CONCURRENT} concurrent, max ${MAX_QUEUE_DEPTH} depth`);
});
