// Readiness probe: poll BASE_URL until HTTP 200 or the deadline expires.
const base = process.env.BASE_URL;
if (!base) {
  console.error('[wait] BASE_URL is not set');
  process.exit(2);
}
const timeoutMs = Number(process.env.READY_TIMEOUT || 60) * 1000;
const start = Date.now();
for (;;) {
  try {
    const res = await fetch(base + '/');
    if (res.ok) {
      console.log(`[wait] app ready after ${((Date.now() - start) / 1000).toFixed(1)}s (HTTP ${res.status})`);
      process.exit(0);
    }
  } catch {
    // not up yet
  }
  if (Date.now() - start > timeoutMs) {
    console.error(`[wait] app at ${base} did not become ready within ${timeoutMs / 1000}s`);
    process.exit(3);
  }
  await new Promise((r) => setTimeout(r, 500));
}
