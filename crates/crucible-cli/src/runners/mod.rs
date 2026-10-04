//! Producing runners (docs/plugins.md §4, slot 1): `crucible run` runs the
//! agent stage by stage, each stage with the runner its taskset names
//! (`runner`, default `workdir`). Only builtin producing runners exist;
//! interactive runners (slot 3) run in the scoring job instead.

use anyhow::{Result, anyhow, bail};
use crucible_core::plugins::{Kind, registry};
use crucible_core::{AgentSpec, TaskSet};
use crucible_metering::Pricing;

use crate::plan::Budget;

pub mod workdir;

pub use workdir::RunArgs;
use workdir::{Env, Used, current_user, read_usage, remaining, run_stage, used};

/// The producing runners built into this crucible.
const BUILTIN: &[&str] = &["workdir"];

/// Check that every stage's runner is a producing runner this build has.
pub fn check(ts: &TaskSet, n: usize) -> Result<()> {
    for s in ts.stages.iter().take(n) {
        let r = registry()
            .resolve(Kind::Runner, s.runner_name())
            .map_err(|e| anyhow!("stage {}: {e}", s.id))?;
        if r.interactive || !r.is_builtin() || !BUILTIN.contains(&r.name.as_str()) {
            bail!("stage {}: runner {} cannot produce an output", s.id, r.name);
        }
    }
    Ok(())
}

pub async fn run(args: RunArgs) -> Result<()> {
    // The credential first: one line on stdin, then stdin is done.
    let cred = crucible_meter::read_credential(&mut std::io::stdin().lock())
        .map_err(|e| anyhow!("{e}"))?;
    run_with(&crate::executor::backend()?, &args, &cred, &|_| {}).await
}

/// Run the stages with an opened credential; `on_stage` is called with
/// each stage id as it starts (progress reports).
pub async fn run_with<E: crate::executor::Executor>(
    exec: &E,
    args: &RunArgs,
    cred: &crucible_meter::Credential,
    on_stage: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let ts: TaskSet = crate::taskset_cmd::load(&args.taskset)?;
    ts.validate(crucible_core::taskset::MAX_TOTAL_TIME_S)?;
    let agent: AgentSpec = serde_json::from_slice(&std::fs::read(&args.agent_json)?)
        .map_err(|_| anyhow!("agent.json is not valid"))?;
    agent.validate()?;
    let n = args.stages.unwrap_or(ts.stages.len());
    if n == 0 || n > ts.stages.len() {
        bail!("--stages must be 1..={}", ts.stages.len());
    }
    check(&ts, n)?;
    if !crate::plan::model_ok(&args.model) {
        bail!("--model has unsupported characters");
    }
    let budget = Budget::parse(&args.budget)?;
    let user_price = match &budget.price {
        Some(p) => crucible_metering::parse_price(p)?,
        None => None,
    };
    let pricing = Pricing::from_json(&std::fs::read_to_string(&args.pricing)?)?;
    let user = match &args.user {
        Some(u) => u.clone(),
        None => current_user()?,
    };
    std::fs::create_dir_all(&args.out_dir)?;
    let scratch = std::fs::canonicalize({
        std::fs::create_dir_all(&args.scratch_dir)?;
        &args.scratch_dir
    })?;
    let env = Env {
        exec,
        args,
        cred,
        agent: &agent,
        pricing: &pricing,
        user_price,
        user,
        work: scratch.join("work"),
        home: scratch.join("home"),
        snaps: scratch.join("snapshots"),
    };
    for d in [&env.work, &env.home, &env.snaps] {
        std::fs::create_dir_all(d)?;
    }

    let mut spent = Used::default();
    for stage in ts.stages.iter().take(n) {
        let limits = remaining(&budget, spent);
        on_stage(&stage.id);
        let (_timing, interrupted) = match stage.runner_name() {
            "workdir" => run_stage(&env, stage, limits).await?,
            other => bail!("runner {other} is not built in"),
        };
        let u = used(&read_usage(
            &args.out_dir.join(&stage.id).join("usage.jsonl"),
        ));
        spent.requests += u.requests;
        spent.tokens += u.tokens;
        spent.cost += u.cost;
        if interrupted {
            bail!("interrupted during stage {}", stage.id);
        }
    }
    Ok(())
}
