//! Packagers (docs/plugins.md §5): turn a stage's work dir into the stage
//! output, and check an uploaded output against the same format. Only
//! builtin Rust implementations exist: packaging handles untrusted bytes
//! on a machine that holds the platform key, so it never runs scripts and
//! never follows a link out of the work dir (`zipdir`).
//!
//! A taskset names its packager per stage (`packager`, formerly `output`);
//! the name must be in the plugin registry (`plugins.json`).

use std::io::Cursor;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use crucible_core::plugins::{Kind, registry};
use serde_json::Value;

use crate::zipdir::{self, ExtractLimits, ZipStats};

mod files;
mod web_app;

/// Upper bound on what an output may contain before compression.
pub const MAX_OUTPUT_BYTES: u64 = 1 << 30;

/// Limits for checking an uploaded output (the same as unpacking one).
pub const CHECK_LIMITS: ExtractLimits = ExtractLimits {
    max_files: 100_000,
    max_bytes: MAX_OUTPUT_BYTES,
};

pub trait Packager: Sync {
    /// generate job: zip the work dir. Reads bytes only; nothing in the
    /// work dir is executed. `app_start_cmd` comes from agent.json.
    fn package(
        &self,
        work: &Path,
        app_start_cmd: &[String],
        opts: &Value,
    ) -> Result<(Vec<u8>, ZipStats)>;
    /// The names (file paths) of an output; is it in this format?
    fn check_names(&self, names: &[String], opts: &Value) -> Result<()>;
    /// Are these `packager_options` meaningful for this packager?
    fn check_options(&self, opts: &Value) -> Result<()>;
}

/// The packager registered under `name`.
pub fn get(name: &str) -> Result<&'static dyn Packager> {
    let p = registry()
        .resolve(Kind::Packager, name)
        .map_err(|e| anyhow!("{e}"))?;
    match p.name.as_str() {
        "web-app" => Ok(&web_app::WebApp),
        "files" => Ok(&files::Files),
        other => bail!("packager {other} is registered but not built into this crucible"),
    }
}

fn opts_or_empty(opts: Option<&Value>) -> Value {
    opts.cloned()
        .unwrap_or_else(|| Value::Object(Default::default()))
}

/// Package `work` with the packager `name`.
pub fn package(
    name: &str,
    work: &Path,
    app_start_cmd: &[String],
    opts: Option<&Value>,
) -> Result<(Vec<u8>, ZipStats)> {
    get(name)?.package(work, app_start_cmd, &opts_or_empty(opts))
}

/// App mode: is an uploaded output a safe zip in the packager's format?
pub fn check(name: &str, artifact: &[u8], opts: Option<&Value>) -> Result<()> {
    let names = zipdir::check_names(Cursor::new(artifact), CHECK_LIMITS)?;
    get(name)?.check_names(&names, &opts_or_empty(opts))
}

/// Taskset registration: the packager exists and its options are valid.
pub fn check_options(name: &str, opts: Option<&Value>) -> Result<()> {
    get(name)?.check_options(&opts_or_empty(opts))
}

/// Refuse an output over [`MAX_OUTPUT_BYTES`] before zipping it.
fn size_ok(stats: &ZipStats) -> Result<()> {
    if stats.bytes > MAX_OUTPUT_BYTES {
        bail!(
            "output is {} bytes, more than the {} byte limit",
            stats.bytes,
            MAX_OUTPUT_BYTES
        );
    }
    Ok(())
}

/// No options at all.
fn no_options(opts: &Value) -> Result<()> {
    match opts.as_object() {
        Some(o) if o.is_empty() => Ok(()),
        _ => bail!("this packager takes no packager_options"),
    }
}
