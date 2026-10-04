// Hostile test pack: what an uploaded (untrusted) test could try from the
// test container. Every test passes only if the attempt is blocked; the
// last one checks the app under test is still reachable. Run by test/e2e.sh
// with CRUCIBLE_SCORER_FIREWALL=1 (docs/scorer-contract.md §7).
import { expect, test } from '@playwright/test';
import * as dns from 'node:dns/promises';
import * as fs from 'node:fs';
import * as net from 'node:net';
import * as os from 'node:os';

type Outcome = 'connected' | 'refused' | 'blocked';

// connected / refused (RST: the host answered) both mean reachable.
function tcp(host: string, port: number, ms = 4000): Promise<Outcome> {
  return new Promise((resolve) => {
    const s = net.connect({ host, port });
    const done = (o: Outcome) => { s.destroy(); resolve(o); };
    s.setTimeout(ms, () => done('blocked'));
    s.once('connect', () => done('connected'));
    s.once('error', (e: NodeJS.ErrnoException) => done(e.code === 'ECONNREFUSED' ? 'refused' : 'blocked'));
  });
}

// The host side of this container's network: the first address of its subnet.
function gateway(): string {
  for (const addrs of Object.values(os.networkInterfaces())) {
    for (const a of addrs ?? []) {
      if (a.family !== 'IPv4' || a.internal) continue;
      const ip = a.address.split('.').map(Number);
      const mask = a.netmask.split('.').map(Number);
      const gw = ip.map((b, i) => b & mask[i]);
      gw[3] += 1;
      return gw.join('.');
    }
  }
  throw new Error('no IPv4 interface');
}

test('no secrets in the environment', async () => {
  const bad = Object.keys(process.env).filter((k) =>
    /TOKEN|SECRET|PASSW|CRED|KEY|^ACTIONS_|^GITHUB_|^RUNNER_/i.test(k),
  );
  expect(bad).toEqual([]);
});

test('unprivileged: non-root, no capabilities, no-new-privileges', async () => {
  const status = fs.readFileSync('/proc/self/status', 'utf8');
  const field = (n: string) => status.match(new RegExp(`^${n}:\\s*(\\S+)`, 'm'))?.[1];
  expect(process.getuid?.()).not.toBe(0);
  expect(field('CapEff')).toBe('0000000000000000');
  expect(field('CapBnd')).toBe('0000000000000000');
  expect(field('NoNewPrivs')).toBe('1');
});

test('no internet', async () => {
  expect(await tcp('1.1.1.1', 443)).toBe('blocked');
  expect(await tcp('140.82.112.3', 443)).toBe('blocked'); // github.com
  await expect(dns.lookup('example.com')).rejects.toThrow();
});

test('no host, runner services or metadata endpoints', async () => {
  const gw = gateway();
  for (const port of [22, 53, 80, 2375, 2376, 8080]) {
    expect(`${gw}:${port} ${await tcp(gw, port)}`).toBe(`${gw}:${port} blocked`);
  }
  for (const [h, p] of [['169.254.169.254', 80], ['168.63.129.16', 80], ['168.63.129.16', 32526]] as const) {
    expect(`${h}:${p} ${await tcp(h, p)}`).toBe(`${h}:${p} blocked`);
  }
});

test('the app under test is reachable', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Hello, world' })).toBeVisible();
});
