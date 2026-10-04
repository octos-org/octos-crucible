//! `crucible ctr`: the container commands of plugin scripts (scorers'
//! `score.sh`, interactive runners' `run.sh`), docs/executors.md §2.3.
//! The scripts call `"$CRUCIBLE" ctr run ...` where they used to call
//! `docker run ...`, with the same flags, and the step's execution backend
//! (`CRUCIBLE_EXECUTOR`) runs them; the scripts no longer know whether it
//! is Docker or Kubernetes.
//!
//! The flags are a fixed subset of docker's, parsed into the backend-
//! neutral [`ContainerSpec`] / [`BuildSpec`]; anything else is refused
//! (no privileged mode, host networking, other security options, devices,
//! or `-e NAME` taking a value from this process's environment).
//!
//! ```text
//! ctr run [-d] [--rm] [flags] IMAGE [ARG...]   exit status: the container's
//! ctr build [-q] [--network=none] [--platform P] [--label K=V] -t TAG DIR
//! ctr logs [--tail N] NAME
//! ctr inspect -f running|exit-code NAME
//! ctr rm [-f] NAME...
//! ctr image exists|pull|rm IMAGE
//! ctr net create [--internal] [--label K=V] NAME | net connect [--alias A] NET NAME | net rm NAME
//! ctr volume create [--label K=V] NAME | volume rm [-f] NAME
//! ctr prune-build-cache
//! ```

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use crate::executor::{
    BuildSpec, ContainerSpec, Executor, Mount, MountSrc, NetSpec, Network, RUN_LABEL, Tmpfs,
};

/// `ctr run` beyond the container itself.
#[derive(Debug, Default, PartialEq)]
pub struct RunOpts {
    pub detach: bool,
    pub rm: bool,
}

fn label(v: &str) -> Result<(String, String)> {
    match v.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.into(), v.into())),
        _ => bail!("--label must be KEY=VALUE"),
    }
}

/// Split `--flag=value` / `--flag value`; `None` for a boolean flag.
struct Args {
    v: Vec<String>,
    i: usize,
}

impl Args {
    fn new(v: &[String]) -> Args {
        Args {
            v: v.to_vec(),
            i: 0,
        }
    }

    /// The next flag (name, inline value) or `None` at the first operand.
    fn flag(&mut self) -> Option<(String, Option<String>)> {
        let a = self.v.get(self.i)?;
        if a == "--" {
            self.i += 1;
            return None;
        }
        if !a.starts_with('-') || a == "-" {
            return None;
        }
        self.i += 1;
        Some(match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_owned(), Some(v.to_owned())),
            _ => (a.clone(), None),
        })
    }

    fn value(&mut self, flag: &str, inline: Option<String>) -> Result<String> {
        if let Some(v) = inline {
            return Ok(v);
        }
        let v = self
            .v
            .get(self.i)
            .cloned()
            .ok_or_else(|| anyhow!("{flag} needs a value"))?;
        self.i += 1;
        Ok(v)
    }

    fn rest(&self) -> &[String] {
        &self.v[self.i..]
    }
}

fn parse_mount(v: &str) -> Result<Result<Mount, Tmpfs>> {
    let (mut kind, mut src, mut dst, mut ro, mut tmp) = ("bind", None, None, false, Vec::new());
    for part in v.split(',') {
        let (k, val) = part.split_once('=').unwrap_or((part, ""));
        match k {
            "type" => {
                kind = match val {
                    "bind" => "bind",
                    "volume" => "volume",
                    "tmpfs" => "tmpfs",
                    _ => bail!("--mount type={val} is not supported"),
                }
            }
            "source" | "src" => src = Some(val.to_owned()),
            "target" | "destination" | "dst" => dst = Some(val.to_owned()),
            "readonly" | "ro" if val.is_empty() || val == "true" => ro = true,
            "tmpfs-mode" | "tmpfs-size" => tmp.push(part.to_owned()),
            _ => bail!("--mount option {k} is not supported"),
        }
    }
    let dst = dst.ok_or_else(|| anyhow!("--mount needs a target"))?;
    Ok(match kind {
        "tmpfs" => Err(Tmpfs {
            dst,
            opts: tmp.join(","),
            via_mount: true,
        }),
        _ => {
            let src = src.ok_or_else(|| anyhow!("--mount needs a source"))?;
            Ok(Mount {
                src: if kind == "bind" {
                    MountSrc::Host(PathBuf::from(src))
                } else {
                    MountSrc::Volume(src)
                },
                dst,
                read_only: ro,
            })
        }
    })
}

fn parse_volume(v: &str) -> Result<Mount> {
    let parts: Vec<&str> = v.split(':').collect();
    let (src, dst, ro) = match parts.as_slice() {
        [s, d] => (*s, *d, false),
        [s, d, "ro"] => (*s, *d, true),
        [s, d, "rw"] => (*s, *d, false),
        _ => bail!("-v must be SRC:DST[:ro|rw]"),
    };
    Ok(Mount {
        src: if src.starts_with('/') {
            MountSrc::Host(PathBuf::from(src))
        } else {
            MountSrc::Volume(src.to_owned())
        },
        dst: dst.to_owned(),
        read_only: ro,
    })
}

/// `ctr run` arguments: the container and how to run it.
pub fn parse_run(argv: &[String]) -> Result<(ContainerSpec, RunOpts)> {
    let mut c = ContainerSpec::default();
    let mut o = RunOpts::default();
    let mut a = Args::new(argv);
    let mut aliases = Vec::new();
    let mut net: Option<String> = None;
    let mut entrypoint = None;
    while let Some((f, inline)) = a.flag() {
        match f.as_str() {
            "-d" | "--detach" => o.detach = true,
            "--rm" => o.rm = true,
            "--init" => c.init = true,
            "--read-only" => c.read_only = true,
            _ => {
                let v = a.value(&f, inline)?;
                match f.as_str() {
                    "--name" => c.name = v,
                    "--label" | "-l" => c.labels.push(label(&v)?),
                    "--network" | "--net" => net = Some(v),
                    "--network-alias" => aliases.push(v),
                    "--dns" => c.dns = Some(v),
                    "--platform" => c.platform = Some(v),
                    "--user" | "-u" => c.user = Some(v),
                    "--memory" | "-m" => c.limits.memory = Some(v),
                    "--memory-swap" => c.limits.memory_swap = Some(v),
                    "--cpus" => c.limits.cpus = Some(v),
                    "--pids-limit" => {
                        c.limits.pids = Some(v.parse().map_err(|_| anyhow!("--pids-limit"))?)
                    }
                    "--cap-drop" => c.cap_drop.push(v),
                    "--cap-add" => c.cap_add.push(v),
                    "--security-opt" => match v.as_str() {
                        "no-new-privileges" | "no-new-privileges:true" => {
                            c.no_new_privileges = true
                        }
                        _ => bail!("--security-opt {v} is not supported"),
                    },
                    "--tmpfs" => {
                        let (dst, opts) = v.split_once(':').unwrap_or((&v, ""));
                        c.tmpfs.push(Tmpfs {
                            dst: dst.into(),
                            opts: opts.into(),
                            via_mount: false,
                        });
                    }
                    "--mount" => match parse_mount(&v)? {
                        Ok(m) => c.mounts.push(m),
                        Err(t) => c.tmpfs.push(t),
                    },
                    "-v" | "--volume" => c.mounts.push(parse_volume(&v)?),
                    "--shm-size" => c.shm_size = Some(v),
                    "--log-opt" => c.log_opts.push(v),
                    "--workdir" | "-w" => c.workdir = Some(v),
                    "--env" | "-e" => match v.split_once('=') {
                        Some((k, val)) if !k.is_empty() => c.env.push((k.into(), val.into())),
                        _ => bail!("-e must be NAME=VALUE"),
                    },
                    "--entrypoint" => entrypoint = Some(v),
                    _ => bail!("ctr run: {f} is not supported"),
                }
            }
        }
    }
    let rest = a.rest();
    let Some(image) = rest.first() else {
        bail!("ctr run: no image");
    };
    c.image = image.clone();
    c.args = rest[1..].to_vec();
    if let Some(ep) = entrypoint {
        c.entrypoint = Some(vec![ep]);
    }
    c.network = match net.as_deref() {
        None => {
            if !aliases.is_empty() {
                bail!("--network-alias needs --network");
            }
            Network::Default
        }
        Some("none") => Network::None,
        Some("host") | Some("bridge") | Some("default") => {
            bail!("--network {}: not supported", net.unwrap_or_default())
        }
        Some(n) => match n.strip_prefix("container:") {
            Some(id) => Network::Container(id.into()),
            None => Network::Named {
                name: n.into(),
                aliases,
            },
        },
    };
    if o.detach && o.rm {
        bail!("ctr run: -d and --rm together are not supported");
    }
    // Everything a step's scripts start carries the step's run label, so
    // the step's cleanup also removes what a killed script left.
    if let Ok(l) = std::env::var("CRUCIBLE_RUN_LABEL")
        && !l.is_empty()
        && !c.labels.iter().any(|(k, _)| k == RUN_LABEL)
    {
        c.labels.push((RUN_LABEL.into(), l));
    }
    Ok((c, o))
}

pub fn parse_build(argv: &[String]) -> Result<BuildSpec> {
    let mut b = BuildSpec::default();
    let mut a = Args::new(argv);
    while let Some((f, inline)) = a.flag() {
        match f.as_str() {
            "-q" | "--quiet" => b.quiet = true,
            _ => {
                let v = a.value(&f, inline)?;
                match f.as_str() {
                    "-t" | "--tag" => b.tag = v,
                    "--network" if v == "none" => b.no_network = true,
                    "--platform" => b.platform = Some(v),
                    "--label" => b.labels.push(label(&v)?),
                    "--build-arg" => b.build_args.push(v),
                    "--progress" if v == "plain" => b.plain_progress = true,
                    "--memory" | "--memory-swap" | "--cpu-quota" | "--cpu-period" => {
                        b.limits.push((f.clone(), v))
                    }
                    _ => bail!("ctr build: {f} {v} is not supported"),
                }
            }
        }
    }
    match a.rest() {
        [dir] if !b.tag.is_empty() => b.dir = PathBuf::from(dir),
        _ => bail!("ctr build: -t TAG DIR"),
    }
    Ok(b)
}

/// Booleans given, `(flag, value)` pairs, operands.
type Parsed = (Vec<String>, Vec<(String, String)>, Vec<String>);

/// Flags of the small commands: `(booleans, values)` and operands.
fn simple(argv: &[String], bools: &[&str], values: &[&str]) -> Result<Parsed> {
    let mut a = Args::new(argv);
    let (mut b, mut v) = (Vec::new(), Vec::new());
    while let Some((f, inline)) = a.flag() {
        if bools.contains(&f.as_str()) {
            b.push(f);
        } else if values.contains(&f.as_str()) {
            let x = a.value(&f, inline)?;
            v.push((f, x));
        } else {
            bail!("{f} is not supported");
        }
    }
    Ok((b, v, a.rest().to_vec()))
}

fn one(ops: &[String], what: &str) -> Result<String> {
    match ops {
        [x] => Ok(x.clone()),
        _ => bail!("{what}"),
    }
}

/// Run `crucible ctr <argv>`; the process exit status.
pub async fn run(argv: &[String]) -> Result<i32> {
    let exec = crate::executor::backend()?;
    let Some((verb, rest)) = argv.split_first() else {
        bail!("ctr: run | build | logs | inspect | rm | image | net | volume | prune-build-cache");
    };
    Ok(match verb.as_str() {
        "run" => {
            let (c, o) = parse_run(rest)?;
            if o.detach {
                println!("{}", exec.start(&c).await?);
                0
            } else {
                exec.run_attached(&c, o.rm).await?
            }
        }
        "build" => exec.build_attached(&parse_build(rest)?).await?,
        "logs" => {
            let (_, v, ops) = simple(rest, &[], &["--tail", "-n"])?;
            let tail = match v.last() {
                Some((_, t)) => Some(t.parse().map_err(|_| anyhow!("--tail N"))?),
                None => None,
            };
            exec.print_logs(&one(&ops, "ctr logs NAME")?, tail).await?
        }
        "inspect" => {
            let (_, v, ops) = simple(rest, &[], &["-f", "--format"])?;
            let id = one(&ops, "ctr inspect -f running|exit-code NAME")?;
            let st = match exec.inspect(&id).await {
                Ok(s) => s,
                Err(_) => return Ok(1),
            };
            match v.last().map(|(_, f)| f.as_str()) {
                Some("running") => println!("{}", st.running),
                Some("exit-code") => {
                    println!(
                        "{}",
                        st.exit_code.map(|c| c.to_string()).unwrap_or_default()
                    )
                }
                _ => bail!("ctr inspect -f running|exit-code NAME"),
            }
            0
        }
        "rm" => {
            let (_, _, ops) = simple(rest, &["-f", "--force"], &[])?;
            for id in &ops {
                exec.remove(id).await;
            }
            0
        }
        "image" => {
            let (sub, rest) = rest
                .split_first()
                .ok_or_else(|| anyhow!("ctr image exists|pull|rm IMAGE"))?;
            let (_, _, ops) = simple(rest, &["-q", "-f"], &[])?;
            let image = one(&ops, "ctr image exists|pull|rm IMAGE")?;
            match sub.as_str() {
                "exists" => i32::from(!exec.image_exists(&image).await),
                "pull" => i32::from(exec.image_pull(&image).await.is_err()),
                "rm" => {
                    exec.image_rm(&image).await;
                    0
                }
                _ => bail!("ctr image exists|pull|rm IMAGE"),
            }
        }
        "net" => {
            let (sub, rest) = rest
                .split_first()
                .ok_or_else(|| anyhow!("ctr net create|connect|rm"))?;
            match sub.as_str() {
                "create" => {
                    let (b, v, ops) = simple(rest, &["--internal"], &["--label"])?;
                    if b.is_empty() {
                        bail!("ctr net create: only --internal networks");
                    }
                    let labels = v.iter().map(|(_, l)| label(l)).collect::<Result<_>>()?;
                    exec.net_create(&NetSpec {
                        name: one(&ops, "ctr net create --internal NAME")?,
                        internal: true,
                        labels,
                    })
                    .await?;
                    0
                }
                "connect" => {
                    let (_, v, ops) = simple(rest, &[], &["--alias"])?;
                    let aliases: Vec<String> = v.into_iter().map(|(_, a)| a).collect();
                    match ops.as_slice() {
                        [net, id] => exec.net_connect(net, id, &aliases).await?,
                        _ => bail!("ctr net connect [--alias A] NET NAME"),
                    }
                    0
                }
                "rm" => {
                    exec.net_rm(&one(rest, "ctr net rm NAME")?).await;
                    0
                }
                _ => bail!("ctr net create|connect|rm"),
            }
        }
        "volume" => {
            let (sub, rest) = rest
                .split_first()
                .ok_or_else(|| anyhow!("ctr volume create|rm"))?;
            match sub.as_str() {
                "create" => {
                    let (_, v, ops) = simple(rest, &[], &["--label"])?;
                    let labels: Vec<_> = v.iter().map(|(_, l)| label(l)).collect::<Result<_>>()?;
                    exec.volume_create(&one(&ops, "ctr volume create NAME")?, &labels)
                        .await?;
                    0
                }
                "rm" => {
                    let (_, _, ops) = simple(rest, &["-f", "--force"], &[])?;
                    exec.volume_rm(&one(&ops, "ctr volume rm NAME")?).await;
                    0
                }
                _ => bail!("ctr volume create|rm"),
            }
        }
        "prune-build-cache" => {
            exec.prune_build_cache().await;
            0
        }
        other => bail!("ctr {other}: unknown command"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::docker::DockerExecutor;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    /// Each `ctr run` of the scripts becomes the `docker run` they ran
    /// before (same flags; mounts in `--mount` form).
    #[test]
    fn script_runs_render_as_before() {
        let cases: &[(&[&str], &[&str])] = &[
            // playwright: the app container
            (
                &[
                    "-d",
                    "--name",
                    "crucible-app-1",
                    "--network",
                    "crucible-net-1",
                    "--network-alias",
                    "app",
                    "--label",
                    "crucible.scorer.run=1",
                    "-e",
                    "PORT=3000",
                    "--memory=512m",
                    "--memory-swap=512m",
                    "--cpus=1.0",
                    "--pids-limit=256",
                    "--tmpfs",
                    "/tmp:rw,noexec,size=64m",
                    "--security-opt",
                    "no-new-privileges",
                    "--cap-drop",
                    "NET_RAW",
                    "--cap-drop",
                    "MKNOD",
                    "--log-opt",
                    "max-size=10m",
                    "img",
                ],
                &[
                    "run",
                    "-d",
                    "--name",
                    "crucible-app-1",
                    "--label",
                    "crucible.scorer.run=1",
                    "--network",
                    "crucible-net-1",
                    "--network-alias",
                    "app",
                    "--memory",
                    "512m",
                    "--memory-swap",
                    "512m",
                    "--cpus",
                    "1.0",
                    "--pids-limit",
                    "256",
                    "--cap-drop",
                    "NET_RAW",
                    "--cap-drop",
                    "MKNOD",
                    "--security-opt",
                    "no-new-privileges",
                    "--tmpfs",
                    "/tmp:rw,noexec,size=64m",
                    "--log-opt",
                    "max-size=10m",
                    "--env",
                    "PORT=3000",
                    "--",
                    "img",
                ],
            ),
            // playwright: a helper (read-only, no network, entrypoint)
            (
                &[
                    "--rm",
                    "--network",
                    "none",
                    "--read-only",
                    "--tmpfs",
                    "/tmp:rw,size=64m",
                    "--security-opt",
                    "no-new-privileges",
                    "--cap-drop",
                    "ALL",
                    "--user",
                    "1000:1000",
                    "--entrypoint",
                    "node",
                    "-v",
                    "/w/app.zip:/in/app.zip:ro",
                    "-v",
                    "/w/out:/out",
                    "img",
                    "/opt/scorer/src/cli.ts",
                    "unpack",
                    "/in/app.zip",
                    "/out",
                ],
                &[
                    "run",
                    "--rm",
                    "--network",
                    "none",
                    "--user",
                    "1000:1000",
                    "--cap-drop",
                    "ALL",
                    "--security-opt",
                    "no-new-privileges",
                    "--read-only",
                    "--tmpfs",
                    "/tmp:rw,size=64m",
                    "--mount",
                    "type=bind,source=/w/app.zip,target=/in/app.zip,readonly",
                    "--mount",
                    "type=bind,source=/w/out,target=/out",
                    "--entrypoint",
                    "node",
                    "--",
                    "img",
                    "/opt/scorer/src/cli.ts",
                    "unpack",
                    "/in/app.zip",
                    "/out",
                ],
            ),
            // arcbench-official: the test container
            (
                &[
                    "--name",
                    "t",
                    "--network",
                    "container:s",
                    "--platform",
                    "linux/amd64",
                    "--mount",
                    "type=tmpfs,destination=/workspace,tmpfs-mode=1777",
                    "--shm-size",
                    "1g",
                    "-v",
                    "ws-1:/workspace2",
                    "img",
                ],
                &[
                    "run",
                    "--name",
                    "t",
                    "--network",
                    "container:s",
                    "--platform",
                    "linux/amd64",
                    "--mount",
                    "type=tmpfs,destination=/workspace,tmpfs-mode=1777",
                    "--shm-size",
                    "1g",
                    "--mount",
                    "type=volume,source=ws-1,target=/workspace2",
                    "--",
                    "img",
                ],
            ),
        ];
        for (input, want) in cases {
            let (c, o) = parse_run(&v(input)).unwrap();
            assert_eq!(
                DockerExecutor::run_args_with(&c, o.detach, o.rm),
                v(want),
                "{input:?}"
            );
        }
    }

    #[test]
    fn dangerous_flags_are_refused() {
        for bad in [
            &["--privileged", "img"][..],
            &["--network", "host", "img"],
            &["--security-opt", "seccomp=unconfined", "img"],
            &["--device", "/dev/kvm", "img"],
            &["-e", "SECRET", "img"],
            &["--pid", "host", "img"],
            &["--volumes-from", "x", "img"],
        ] {
            assert!(parse_run(&v(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn builds_render_as_before() {
        let b = parse_build(&v(&[
            "--network=none",
            "--memory=2g",
            "--memory-swap=2g",
            "--cpu-quota=200000",
            "--cpu-period=100000",
            "--label",
            "crucible.scorer.run=1",
            "-t",
            "app-1",
            "/w/src",
        ]))
        .unwrap();
        assert_eq!(
            DockerExecutor::build_args(&b),
            v(&[
                "build",
                "--network=none",
                "--memory=2g",
                "--memory-swap=2g",
                "--cpu-quota=200000",
                "--cpu-period=100000",
                "--label",
                "crucible.scorer.run=1",
                "-t",
                "app-1",
                "--",
                "/w/src",
            ])
        );
        let b = parse_build(&v(&[
            "-q",
            "--platform",
            "linux/amd64",
            "-t",
            "s:local",
            "d",
        ]))
        .unwrap();
        assert_eq!(
            DockerExecutor::build_args(&b),
            v(&[
                "build",
                "-q",
                "--platform",
                "linux/amd64",
                "-t",
                "s:local",
                "--",
                "d"
            ])
        );
        assert!(parse_build(&v(&["--network", "host", "-t", "x", "d"])).is_err());
    }
}
