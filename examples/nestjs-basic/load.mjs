// Fires 10,000 POST /email/send requests with controlled concurrency.
const URL = 'http://localhost:3000/email/send';
const TOTAL = 10_000;
const CONCURRENCY = 50;

let sent = 0, ok = 0, err = 0;
const latencies = [];
const start = Date.now();

async function worker() {
  while (sent < TOTAL) {
    const i = ++sent;
    const t0 = Date.now();
    try {
      const r = await fetch(URL, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ to: `user${i}@example.com`, subject: `Load test ${i}` }),
      });
      latencies.push(Date.now() - t0);
      if (r.ok) ok++; else err++;
    } catch {
      latencies.push(Date.now() - t0);
      err++;
    }
  }
}

const ticker = setInterval(() => {
  const elapsed = ((Date.now() - start) / 1000).toFixed(1);
  const rps = ((ok + err) / ((Date.now() - start) / 1000)).toFixed(0);
  process.stdout.write(`\r  sent=${ok + err}/${TOTAL}  ok=${ok}  err=${err}  ${rps} req/s  ${elapsed}s`);
}, 250);

await Promise.all(Array.from({ length: CONCURRENCY }, worker));
clearInterval(ticker);

const elapsed = (Date.now() - start) / 1000;
latencies.sort((a, b) => a - b);
const p50 = latencies[Math.floor(latencies.length * 0.50)];
const p95 = latencies[Math.floor(latencies.length * 0.95)];
const p99 = latencies[Math.floor(latencies.length * 0.99)];
const avg = (latencies.reduce((s, v) => s + v, 0) / latencies.length).toFixed(1);

console.log(`\n
  total     ${ok + err} requests  (${ok} ok / ${err} err)
  duration  ${elapsed.toFixed(2)}s
  req/s     ${(TOTAL / elapsed).toFixed(0)} avg

  latency (ms)
    avg  ${avg}
    p50  ${p50}
    p95  ${p95}
    p99  ${p99}
    max  ${latencies[latencies.length - 1]}
`);
