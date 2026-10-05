//! `crucible eval nomad`: the same driver as `eval local`, but every step is
//! a Nomad batch job (docs/executors.md §3.3 "整步托管", docs/nomad.md).
//!
//! Nomad only places the step: the job's one task runs `crucible step
//! <name>` with `raw_exec` on a node, and the step starts its containers
//! through that node's Docker exactly as on one machine (same sandbox
//! slots, same network rules, same cleanup).
//!
//! Secrets: before submitting, the driver writes the step's secrets into
//! the job's own Nomad Variable `nomad/jobs/<job id>` (one item per
//! secret, base64), and the job renders each with a `template` into
//! `secrets/crucible/<name>` in the task's secrets directory, which the
//! step reads with `--secrets-dir`. The job spec and the variable are both
//! built from the step's list ([`StepSpec::secrets`]); a secret outside it
//! is refused, so `score-tests` can only ever be given the one-run key.
//! When the job ends the driver deletes the variable and purges the job.
//!
//! Bundles: the evaluation directory, the store (`dir:`), the repository
//! and the `crucible` binary are paths the nodes see at the same place
//! (one node: this machine's disk; several: a shared mount).

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::steps::{Pool, Secret, StepSpec};

#[derive(clap::Args)]
pub struct NomadArgs {
    #[command(flatten)]
    pub eval: crate::local::LocalArgs,
    /// The Nomad HTTP API.
    #[arg(long, env = "NOMAD_ADDR", default_value = "http://127.0.0.1:4646")]
    pub addr: String,
    /// A Nomad ACL token allowed to submit and purge `crucible-*` jobs and
    /// write `nomad/jobs/crucible-*` variables (none when ACLs are off).
    #[arg(long, env = "NOMAD_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// The `crucible` binary as the nodes see it (default: this one).
    #[arg(long)]
    pub crucible_bin: Option<std::path::PathBuf>,
    /// Node pool of the steps that start no untrusted code.
    #[arg(long, default_value = "default")]
    pub trusted_pool: String,
    /// Node pool of the steps that run agents or uploaded tests.
    #[arg(long, default_value = "default")]
    pub sandbox_pool: String,
}

/// Where and how jobs are submitted.
pub struct Nomad {
    client: reqwest::Client,
    addr: String,
    token: Option<String>,
    pub trusted_pool: String,
    pub sandbox_pool: String,
    /// Environment of every step task (HOME, PATH): raw_exec tasks run as
    /// the Nomad client's user with a minimal environment.
    pub env: Vec<(String, String)>,
}

const TASK: &str = "step";

/// The variable item of a secret (Go template field names: no `-`).
fn item(s: Secret) -> String {
    s.name().replace('-', "_")
}

/// The variable path a job reads (Nomad's default workload identity may
/// read `nomad/jobs/<its job id>`).
pub fn var_path(job_id: &str) -> String {
    format!("nomad/jobs/{job_id}")
}

/// The Variable holding exactly `secrets`, all of them in `step`'s list.
pub fn var_body(step: &StepSpec, job_id: &str, secrets: &[(Secret, &[u8])]) -> Result<Value> {
    let mut items = serde_json::Map::new();
    for (s, v) in secrets {
        if !step.secrets.contains(s) {
            bail!("step {} may not hold {}", step.name, s.name());
        }
        items.insert(
            item(*s),
            Value::String(base64::engine::general_purpose::STANDARD.encode(v)),
        );
    }
    Ok(json!({"Path": var_path(job_id), "Items": items}))
}

/// The batch job of one step: one task, `raw_exec`, no restarts or
/// reschedules (the driver decides about retries), one template per
/// secret, all of them in `step`'s list.
pub fn job_spec(
    job_id: &str,
    step: &StepSpec,
    pool: &str,
    exe: &Path,
    args: &[String],
    env: &[(String, String)],
    secrets: &[Secret],
) -> Result<Value> {
    let mut templates = Vec::new();
    for s in secrets {
        if !step.secrets.contains(s) {
            bail!("step {} may not hold {}", step.name, s.name());
        }
        templates.push(json!({
            "DestPath": format!("secrets/crucible/{}", s.name()),
            "EmbeddedTmpl": format!(
                "{{{{ with nomadVar \"{}\" }}}}{{{{ .{}.Value | base64Decode }}}}{{{{ end }}}}",
                var_path(job_id),
                item(*s)
            ),
            "Perms": "0400",
            "ChangeMode": "noop",
        }));
    }
    let mut argv = vec![
        "step".to_string(),
        step.name.to_string(),
        "--secrets-dir".into(),
        "${NOMAD_SECRETS_DIR}/crucible".into(),
    ];
    argv.extend(args.iter().cloned());
    let env: serde_json::Map<String, Value> = env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    Ok(json!({
        "ID": job_id,
        "Name": job_id,
        "Type": "batch",
        "Datacenters": ["*"],
        "NodePool": pool,
        "Meta": {"crucible-step": step.name},
        "TaskGroups": [{
            "Name": TASK,
            "Count": 1,
            "RestartPolicy": {"Attempts": 0, "Mode": "fail"},
            "ReschedulePolicy": {"Attempts": 0, "Unlimited": false},
            "Tasks": [{
                "Name": TASK,
                "Driver": "raw_exec",
                "Config": {"command": exe.display().to_string(), "args": argv},
                "Env": env,
                "Templates": templates,
                "Resources": {"CPU": 500, "MemoryMB": 1024},
                "KillTimeout": 30_000_000_000u64,
            }],
        }],
    }))
}

/// A job id for a step of an evaluation (`generate r1` -> `generate-r1`).
pub fn job_id(eval_id: &str, label: &str) -> String {
    format!("crucible-{eval_id}-{}", label.replace(' ', "-"))
}

/// How a step job ended (the times are Nomad's task state).
pub struct Ended {
    pub ok: bool,
    pub started: SystemTime,
    pub ended: SystemTime,
}

impl Nomad {
    pub fn new(a: &NomadArgs) -> Result<Nomad> {
        let mut env = Vec::new();
        // CRUCIBLE_DOCKER_RUNTIME: the nodes' Docker runs the step's
        // containers under it (gVisor), as with `eval local`.
        for k in ["HOME", "PATH", "CRUCIBLE_DOCKER_RUNTIME"] {
            if let Some(v) = std::env::var_os(k) {
                env.push((k.to_string(), v.to_string_lossy().into_owned()));
            }
        }
        Ok(Nomad {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
            addr: a.addr.trim_end_matches('/').to_string(),
            token: a.token.clone().filter(|t| !t.is_empty()),
            trusted_pool: a.trusted_pool.clone(),
            sandbox_pool: a.sandbox_pool.clone(),
            env,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Vec<u8>> {
        let mut last = None;
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let mut r = self
                .client
                .request(method.clone(), format!("{}{path}", self.addr));
            if let Some(t) = &self.token {
                r = r.header("X-Nomad-Token", t);
            }
            if let Some(b) = body {
                r = r
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_vec(b)?);
            }
            match r.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp.bytes().await?.to_vec()),
                Ok(resp) if resp.status().is_server_error() => {
                    last = Some(anyhow!("nomad {method} {path}: {}", resp.status()))
                }
                Ok(resp) => {
                    let st = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    bail!("nomad {method} {path}: {st} {}", t.trim());
                }
                Err(e) => last = Some(anyhow!("nomad {method} {path}: {e}")),
            }
        }
        Err(last.expect("attempted"))
    }

    async fn get(&self, path: &str) -> Result<Value> {
        Ok(serde_json::from_slice(
            &self.call(reqwest::Method::GET, path, None).await?,
        )?)
    }

    /// Run one step as a job: variable, submit, wait, logs, then delete
    /// the variable and purge the job whatever happened.
    pub async fn run_step(
        &self,
        job_id: &str,
        step: &'static StepSpec,
        exe: &Path,
        args: &[String],
        extra_env: &[(String, String)],
        secrets: &[(Secret, &[u8])],
    ) -> Result<Ended> {
        let pool = match step.needs.pool {
            Pool::Trusted => &self.trusted_pool,
            Pool::Sandbox => &self.sandbox_pool,
        };
        let mut env = self.env.clone();
        env.extend(extra_env.iter().cloned());
        let names: Vec<Secret> = secrets.iter().map(|(s, _)| *s).collect();
        let job = job_spec(job_id, step, pool, exe, args, &env, &names)?;
        let var = var_body(step, job_id, secrets)?;
        let vpath = format!("/v1/var/{}", var_path(job_id));
        self.call(reqwest::Method::PUT, &vpath, Some(&var))
            .await
            .context("writing the step's variable")?;
        let r = async {
            self.call(
                reqwest::Method::POST,
                "/v1/jobs",
                Some(&json!({"Job": job})),
            )
            .await
            .context("submitting the step job")?;
            eprintln!("   nomad job {job_id} (pool {pool})");
            self.wait(job_id).await
        }
        .await;
        if let Err(e) = self.call(reqwest::Method::DELETE, &vpath, None).await {
            eprintln!("   warning: deleting variable {vpath}: {e}");
        }
        if let Err(e) = self
            .call(
                reqwest::Method::DELETE,
                &format!("/v1/job/{job_id}?purge=true"),
                None,
            )
            .await
        {
            eprintln!("   warning: purging job {job_id}: {e}");
        }
        r
    }

    async fn wait(&self, job_id: &str) -> Result<Ended> {
        let mut polls = 0u64;
        loop {
            let allocs = self.get(&format!("/v1/job/{job_id}/allocations")).await?;
            let done = allocs.as_array().into_iter().flatten().find(|a| {
                matches!(
                    a["ClientStatus"].as_str(),
                    Some("complete" | "failed" | "lost")
                )
            });
            if let Some(a) = done {
                let id = a["ID"].as_str().unwrap_or_default().to_string();
                let full = self.get(&format!("/v1/allocation/{id}")).await?;
                let ts = &full["TaskStates"][TASK];
                for kind in ["stdout", "stderr"] {
                    match self
                        .call(
                            reqwest::Method::GET,
                            &format!("/v1/client/fs/logs/{id}?task={TASK}&type={kind}&origin=start&plain=true"),
                            None,
                        )
                        .await
                    {
                        Ok(b) => {
                            use std::io::Write;
                            let _ = std::io::stderr().write_all(&b);
                        }
                        Err(e) => eprintln!("   (no {kind} log: {e})"),
                    }
                }
                let ok = a["ClientStatus"] == "complete" && ts["Failed"] == false;
                if !ok {
                    for ev in ts["Events"].as_array().into_iter().flatten() {
                        eprintln!(
                            "   nomad: {} {}",
                            ev["Type"].as_str().unwrap_or(""),
                            ev["DisplayMessage"].as_str().unwrap_or("")
                        );
                    }
                }
                let t = |k: &str| {
                    ts[k]
                        .as_str()
                        .and_then(|s| humantime::parse_rfc3339(s).ok())
                        .unwrap_or_else(SystemTime::now)
                };
                return Ok(Ended {
                    ok,
                    started: t("StartedAt"),
                    ended: t("FinishedAt"),
                });
            }
            polls += 1;
            if polls == 15 && allocs.as_array().is_none_or(|a| a.is_empty()) {
                eprintln!(
                    "   waiting for Nomad to place {job_id} (see `nomad job status {job_id}`)"
                );
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steps::{STEPS, spec};

    fn job(step: &str, secrets: &[Secret]) -> Result<Value> {
        job_spec(
            "crucible-e-x",
            spec(step),
            "default",
            Path::new("/opt/crucible"),
            &["--out".into(), "/x".into()],
            &[],
            secrets,
        )
    }

    fn rendered(j: &Value) -> Vec<String> {
        j["TaskGroups"][0]["Tasks"][0]["Templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["DestPath"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn score_tests_gets_only_the_run_key() {
        let j = job("score-tests", &[Secret::RunKey]).unwrap();
        assert_eq!(rendered(&j), ["secrets/crucible/run-key"]);
        let text = j.to_string();
        for s in Secret::ALL.into_iter().filter(|s| *s != Secret::RunKey) {
            assert!(!text.contains(s.name()), "{} in score-tests job", s.name());
            assert!(!text.contains(&item(s)), "{} in score-tests job", s.name());
        }
        for s in Secret::ALL.into_iter().filter(|s| *s != Secret::RunKey) {
            assert!(job("score-tests", &[s]).is_err());
            assert!(var_body(spec("score-tests"), "j", &[(s, b"v")]).is_err());
        }
        let v = var_body(spec("score-tests"), "j", &[(Secret::RunKey, b"k")]).unwrap();
        assert_eq!(v["Items"].as_object().unwrap().len(), 1);
        assert_eq!(v["Items"]["run_key"], "aw==");
        assert_eq!(v["Path"], "nomad/jobs/j");
    }

    #[test]
    fn every_step_renders_exactly_what_it_is_given_from_its_list() {
        for st in STEPS {
            let j = job(st.name, st.secrets).unwrap();
            let want: Vec<String> = st
                .secrets
                .iter()
                .map(|s| format!("secrets/crucible/{}", s.name()))
                .collect();
            assert_eq!(rendered(&j), want, "{}", st.name);
            for s in Secret::ALL.into_iter().filter(|s| !st.secrets.contains(s)) {
                assert!(job(st.name, &[s]).is_err(), "{} took {}", st.name, s.name());
            }
        }
        let j = job("generate", &[Secret::PlatformKey, Secret::DevModelCred]).unwrap();
        assert_eq!(
            rendered(&j),
            [
                "secrets/crucible/platform-key",
                "secrets/crucible/dev-model-cred"
            ]
        );
        let t = &j["TaskGroups"][0]["Tasks"][0];
        assert_eq!(
            t["Templates"][0]["EmbeddedTmpl"],
            "{{ with nomadVar \"nomad/jobs/crucible-e-x\" }}{{ .platform_key.Value | base64Decode }}{{ end }}"
        );
        assert_eq!(t["Driver"], "raw_exec");
        assert_eq!(
            t["Config"]["args"],
            json!([
                "step",
                "generate",
                "--secrets-dir",
                "${NOMAD_SECRETS_DIR}/crucible",
                "--out",
                "/x"
            ])
        );
        assert_eq!(j["TaskGroups"][0]["RestartPolicy"]["Attempts"], 0);
        assert_eq!(j["TaskGroups"][0]["ReschedulePolicy"]["Attempts"], 0);
    }

    #[test]
    fn job_ids_are_per_evaluation() {
        assert_eq!(
            job_id("nomad-a", "score-tests r2"),
            "crucible-nomad-a-score-tests-r2"
        );
        assert_ne!(job_id("nomad-a", "handoff"), job_id("nomad-b", "handoff"));
    }
}
