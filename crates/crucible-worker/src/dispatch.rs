//! `workflow_dispatch` inputs for an eval. Non-secret values only: the
//! credential stays in KV and the workflow fetches it by `eval_id`.
//!
//! Agent mode targets `eval.yml` with exactly the inputs it declares
//! (GitHub rejects undeclared inputs with 422):
//! `agent_source, taskset, model, endpoint, replicas, cred_source, eval_id,
//! score_public, owner, options` where `options` is JSON
//! `{stages, budget?, results_url}`.
//!
//! App mode targets the scoring workflow (`SCORE_WORKFLOW`, default
//! `score.yml`): `eval_id, artifact_source, taskset, stage, cred_source, model,
//! score_public, owner, results_url`.

use serde_json::{Value, json};

use crate::config::Config;
use crate::model::{EvalRecord, Mode};

/// `cred_source` value telling the workflow to fetch the sealed
/// credential from `GET /internal/cred/:eval_id`.
pub const CRED_FROM_KV: &str = "workers-kv";

pub fn results_url(worker_url: &str, eval_id: &str) -> String {
    format!("{worker_url}/internal/results/{eval_id}")
}

/// `(workflow file, inputs)`.
pub fn inputs(cfg: &Config, rec: &EvalRecord, has_cred: bool, worker_url: &str) -> (String, Value) {
    let owner = format!("{}:{}", rec.owner_id, rec.owner_login);
    let score_public = rec.score_public.to_string();
    let source = format!("blob:{}", rec.upload_hash);
    let results = results_url(worker_url, &rec.eval_id);
    let cred_source = if has_cred { CRED_FROM_KV } else { "none" };
    match rec.mode {
        Mode::Agent => {
            let mut options = json!({"stages": rec.stages, "results_url": results});
            if let Some(b) = &rec.budget {
                options["budget"] = serde_json::to_value(b).expect("json");
            }
            (
                cfg.eval_workflow.clone(),
                json!({
                    "agent_source": source,
                    "taskset": rec.taskset,
                    "model": rec.model.clone().unwrap_or_default(),
                    // The endpoint travels inside the sealed credential.
                    "endpoint": "",
                    "replicas": rec.replicas.to_string(),
                    "cred_source": cred_source,
                    "eval_id": rec.eval_id,
                    "score_public": score_public,
                    "owner": owner,
                    "options": options.to_string(),
                }),
            )
        }
        Mode::App => (
            cfg.score_workflow.clone(),
            json!({
                "eval_id": rec.eval_id,
                "artifact_source": source,
                "taskset": rec.taskset,
                "stage": rec.stages.to_string(),
                "cred_source": cred_source,
                "model": rec.model.clone().unwrap_or_default(),
                "score_public": score_public,
                "owner": owner,
                "results_url": results,
            }),
        ),
    }
}
