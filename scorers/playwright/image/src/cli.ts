// In-image helper for score.sh. Two subcommands, both run with --network none:
//
//   unpack <zip> <dest>
//       Zip safety check + extraction. Exit 0 ok, 1 rejected (the agent's
//       artifact is malformed), anything else = scorer fault.
//   result --status S --detail D --visibility V --report R --out O
//          [--task-id ID] [--submission-id ID]
//       Playwright report.json -> result.json (ScoreResult).

import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { parseArgs } from 'node:util';
import { buildResult, toJson, type InputStatus, type Visibility } from './report.ts';
import { checkAndExtract } from './zip.ts';

function unpack(args: string[]): number {
  if (args.length !== 2) {
    console.error('usage: cli.ts unpack <zip> <dest>');
    return 2;
  }
  try {
    checkAndExtract(args[0], args[1]);
  } catch (err) {
    console.error(`[unpack] ${(err as Error).message}`);
    return 1;
  }
  return 0;
}

const STATUSES = ['scored', 'failed', 'system_error', 'rejected'];

function result(args: string[]): number {
  const { values: v } = parseArgs({
    args,
    options: {
      status: { type: 'string', default: 'system_error' },
      detail: { type: 'string', default: '' },
      visibility: { type: 'string', default: 'public' },
      report: { type: 'string' },
      out: { type: 'string' },
      'task-id': { type: 'string' },
      'submission-id': { type: 'string' },
    },
  });
  if (!v.report || !v.out || !STATUSES.includes(v.status!) || !['public', 'hidden'].includes(v.visibility!)) {
    console.error('usage: cli.ts result --status S --detail D --visibility public|hidden --report R --out O');
    return 2;
  }
  let report: unknown = null;
  if (existsSync(v.report)) {
    try {
      report = JSON.parse(readFileSync(v.report, 'utf8'));
    } catch (err) {
      report = err as Error;
    }
  }
  const r = buildResult({
    submissionId: v['submission-id'],
    taskId: v['task-id'],
    visibility: v.visibility as Visibility,
    status: v.status as InputStatus,
    detail: v.detail!,
    report,
  });
  writeFileSync(v.out, toJson(r));
  return 0;
}

const [cmd, ...rest] = process.argv.slice(2);
let code: number;
if (cmd === 'unpack') code = unpack(rest);
else if (cmd === 'result') code = result(rest);
else {
  console.error('usage: cli.ts unpack|result ...');
  code = 2;
}
process.exit(code);
