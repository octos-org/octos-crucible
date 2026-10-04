-- GET /leaderboard and /leaderboard/:taskset (docs/api.md "排行榜"): only
-- public evals are indexed, so the leaderboard never scans private ones.
CREATE INDEX evals_public_taskset ON evals (taskset, created_s DESC) WHERE score_public = 1;
