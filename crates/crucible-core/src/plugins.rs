//! The plugin registry (`plugins.json` at the repository root, compiled
//! in): runners, packagers and scorers by name (docs/plugins.md §3). Every
//! tool — the command line, the Worker, the taskset-pack job — only knows
//! the names listed here; a taskset can only refer to them, and an
//! uploaded taskset only to those marked `user`.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The registry as compiled into this build.
pub const PLUGINS_JSON: &str = include_str!("../../../plugins.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Runner,
    Packager,
    Scorer,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Runner => "runner",
            Kind::Packager => "packager",
            Kind::Scorer => "scorer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub kind: Kind,
    pub name: String,
    pub version: String,
    /// `builtin` (Rust, compiled into `crucible`) or a directory of the
    /// repository holding the plugin's entry script and `image/`.
    #[serde(rename = "impl")]
    pub implementation: String,
    /// Runners only: an interactive runner (slot 3, in `score-tests`).
    #[serde(default)]
    pub interactive: bool,
    /// Uploaded tasksets may refer to it.
    #[serde(default)]
    pub user: bool,
    /// It executes files of the taskset as code. Defaults to true.
    #[serde(default = "yes")]
    pub runs_taskset_code: bool,
    /// A taskset may give it a model (docs/plugins.md §10).
    #[serde(default)]
    pub model: bool,
    /// Scorers only: the packagers whose output it can score (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts: Vec<String>,
}

fn yes() -> bool {
    true
}

impl Plugin {
    pub fn is_builtin(&self) -> bool {
        self.implementation == "builtin"
    }

    /// A user-uploaded plugin (docs/plugins.md §14).
    pub fn is_user(&self) -> bool {
        self.implementation == USER_IMPL
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema: u32,
    pub plugins: Vec<Plugin>,
}

/// A directory of the repository: `runners/<x>` or `scorers/<x>`.
fn impl_dir_ok(kind: Kind, s: &str) -> bool {
    let prefix = match kind {
        Kind::Runner => "runners/",
        Kind::Scorer => "scorers/",
        Kind::Packager => return false,
    };
    s.strip_prefix(prefix)
        .is_some_and(|n| crate::is_slug(n, 40))
}

impl Registry {
    pub fn parse(raw: &str) -> Result<Registry, String> {
        let r: Registry = serde_json::from_str(raw).map_err(|e| format!("plugins.json: {e}"))?;
        r.check()?;
        Ok(r)
    }

    /// The registry's own rules (§3): names unique per kind, versions
    /// plain, packagers builtin only, and no plugin that runs taskset code
    /// may be given a model.
    pub fn check(&self) -> Result<(), String> {
        if self.schema != 1 {
            return Err("plugins.json: schema must be 1".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.plugins {
            let id = format!("{} {}", p.kind.as_str(), p.name);
            if !crate::is_slug(&p.name, 40) || is_user_plugin_id(&p.name) {
                return Err(format!("{id}: bad name"));
            }
            if !seen.insert((p.kind, p.name.clone())) {
                return Err(format!("{id}: listed twice"));
            }
            if p.version.is_empty()
                || p.version.len() > 20
                || !p
                    .version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.')
            {
                return Err(format!("{id}: bad version"));
            }
            if !(p.is_builtin() || impl_dir_ok(p.kind, &p.implementation)) {
                return Err(format!(
                    "{id}: impl must be builtin or runners/<name>, scorers/<name>"
                ));
            }
            if p.kind == Kind::Packager && !p.is_builtin() {
                return Err(format!("{id}: packagers are builtin only"));
            }
            if p.interactive && p.kind != Kind::Runner {
                return Err(format!("{id}: only runners are interactive"));
            }
            if p.interactive && p.is_builtin() {
                return Err(format!("{id}: an interactive runner is a container plugin"));
            }
            if p.model && p.runs_taskset_code {
                return Err(format!(
                    "{id}: runs taskset code, so it may not have a model"
                ));
            }
            if p.model && !(p.kind == Kind::Scorer || p.interactive) {
                return Err(format!(
                    "{id}: only scorers and interactive runners use a model"
                ));
            }
            if !p.accepts.is_empty() && p.kind != Kind::Scorer {
                return Err(format!("{id}: only scorers have accepts"));
            }
        }
        for p in &self.plugins {
            for a in &p.accepts {
                if self.get(Kind::Packager, a).is_none() {
                    return Err(format!("scorer {}: accepts unknown packager {a}", p.name));
                }
            }
        }
        Ok(())
    }

    pub fn get(&self, kind: Kind, name: &str) -> Option<&Plugin> {
        self.plugins
            .iter()
            .find(|p| p.kind == kind && p.name == name)
    }

    /// Look up `name` or `name@version` (the version must be the
    /// registered one: the registry holds one version of each plugin).
    pub fn resolve(&self, kind: Kind, reference: &str) -> Result<&Plugin, String> {
        let (name, version) = match reference.split_once('@') {
            Some((n, v)) => (n, Some(v)),
            None => (reference, None),
        };
        let p = self
            .get(kind, name)
            .ok_or_else(|| format!("unknown {} {name:?}", kind.as_str()))?;
        if let Some(v) = version
            && v != p.version
        {
            return Err(format!(
                "{} {name}: version {v} is not available (this platform has {})",
                kind.as_str(),
                p.version
            ));
        }
        Ok(p)
    }
}

/// `u-` + 16 lower-case hex: the platform id of a user-uploaded plugin
/// (docs/plugins.md §14). Registry names never look like this.
pub fn is_user_plugin_id(s: &str) -> bool {
    crate::taskset::is_user_taskset_id(s)
}

/// A plugin package's own declaration: `plugin.json` at the root of the
/// uploaded zip, next to its `Dockerfile` (docs/plugins.md §14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub schema: u32,
    /// Only `scorer` for now.
    pub kind: Kind,
    /// The uploader's name for it (`[a-z0-9][a-z0-9-]{0,39}`); the
    /// platform registers it under an id `u-<16 hex>`.
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Whether it executes taskset files as code. Defaults to true.
    #[serde(default = "yes")]
    pub runs_taskset_code: bool,
    /// Whether a taskset may give it the submitter's model (through the meter).
    #[serde(default)]
    pub model: bool,
    /// The packagers whose output it can score (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts: Vec<String>,
}

fn version_ok(v: &str) -> bool {
    !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

impl PluginManifest {
    pub fn parse(raw: &[u8]) -> Result<PluginManifest, String> {
        let m: PluginManifest =
            serde_json::from_slice(raw).map_err(|e| format!("plugin.json: {e}"))?;
        m.check()?;
        Ok(m)
    }

    pub fn check(&self) -> Result<(), String> {
        if self.schema != 1 {
            return Err("plugin.json: schema must be 1".into());
        }
        if self.kind != Kind::Scorer {
            return Err("plugin.json: only scorer plugins can be uploaded for now".into());
        }
        if !crate::is_slug(&self.name, 40) {
            return Err("plugin.json: name must match [a-z0-9][a-z0-9-]{0,39}".into());
        }
        if !version_ok(&self.version) {
            return Err("plugin.json: version must be 1-20 of [A-Za-z0-9.]".into());
        }
        if self.description.chars().count() > 300 || self.description.chars().any(char::is_control)
        {
            return Err("plugin.json: description ≤ 300 characters, no control characters".into());
        }
        check_traits(self.runs_taskset_code, self.model, &self.accepts)
            .map_err(|e| format!("plugin.json: {e}"))
    }

    /// The registered form, as a taskset pins it.
    pub fn pin(&self, id: &str, blob: crate::BlobRef) -> UserPlugin {
        UserPlugin {
            kind: self.kind,
            name: id.to_owned(),
            version: self.version.clone(),
            blob,
            runs_taskset_code: self.runs_taskset_code,
            model: self.model,
            accepts: self.accepts.clone(),
        }
    }
}

fn check_traits(runs_taskset_code: bool, model: bool, accepts: &[String]) -> Result<(), String> {
    if model && runs_taskset_code {
        return Err("a plugin that runs taskset code may not have a model".into());
    }
    if accepts.len() > 8 {
        return Err("accepts: at most 8 packagers".into());
    }
    for a in accepts {
        if registry().get(Kind::Packager, a).is_none() {
            return Err(format!("accepts unknown packager {a:?}"));
        }
    }
    Ok(())
}

/// A user-uploaded plugin as a registered taskset pins it (`user_plugins`
/// in taskset.json): its id, version, traits and the sealed package. The
/// platform runs it only inside containers of the scoring job (its image
/// is built there from the package), through the generic shell
/// `scorers/_user/score.sh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPlugin {
    pub kind: Kind,
    /// `u-<16 hex>`.
    pub name: String,
    pub version: String,
    /// The sealed package (a zip: plugin.json, Dockerfile, build context).
    pub blob: crate::BlobRef,
    #[serde(default = "yes")]
    pub runs_taskset_code: bool,
    #[serde(default)]
    pub model: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepts: Vec<String>,
}

/// `impl` of a user plugin seen as a [`Plugin`].
pub const USER_IMPL: &str = "user";

impl UserPlugin {
    pub fn check(&self) -> Result<(), String> {
        let id = format!("{} {}", self.kind.as_str(), self.name);
        if !is_user_plugin_id(&self.name) {
            return Err(format!("{id}: a user plugin id is u-<16 hex>"));
        }
        if self.kind != Kind::Scorer {
            return Err(format!("{id}: only scorer plugins can be uploaded"));
        }
        if !version_ok(&self.version) {
            return Err(format!("{id}: bad version"));
        }
        if !self.blob.is_valid() {
            return Err(format!("{id}: bad blob reference"));
        }
        check_traits(self.runs_taskset_code, self.model, &self.accepts)
            .map_err(|e| format!("{id}: {e}"))
    }

    /// As a registry entry (offered to uploaded tasksets).
    pub fn as_plugin(&self) -> Plugin {
        Plugin {
            kind: self.kind,
            name: self.name.clone(),
            version: self.version.clone(),
            implementation: USER_IMPL.into(),
            interactive: false,
            user: true,
            runs_taskset_code: self.runs_taskset_code,
            model: self.model,
            accepts: self.accepts.clone(),
        }
    }
}

/// The compiled-in registry. Its validity is a unit test, so this never
/// fails in a released build.
pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry::parse(PLUGINS_JSON).expect("plugins.json is checked by tests"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_registry_is_valid() {
        let r = registry();
        assert!(r.get(Kind::Runner, "workdir").unwrap().is_builtin());
        assert!(r.get(Kind::Packager, "web-app").unwrap().user);
        let pw = r.get(Kind::Scorer, "playwright").unwrap();
        assert!(pw.user && pw.runs_taskset_code && !pw.model);
        assert_eq!(pw.implementation, "scorers/playwright");
        assert_eq!(
            r.resolve(Kind::Scorer, "playwright@1").unwrap().name,
            "playwright"
        );
        assert!(r.resolve(Kind::Scorer, "playwright@9").is_err());
        assert!(r.resolve(Kind::Scorer, "nope").is_err());
        assert!(r.resolve(Kind::Packager, "playwright").is_err());
    }

    #[test]
    fn registry_rules() {
        let base = |extra: &str| {
            format!(
                r#"{{"schema":1,"plugins":[{{"kind":"packager","name":"files","version":"1","impl":"builtin"}}{extra}]}}"#
            )
        };
        Registry::parse(&base("")).unwrap();
        for (bad, why) in [
            (
                r#",{"kind":"scorer","name":"x","version":"1","impl":"scorers/x","model":true}"#,
                "model + runs taskset code (default)",
            ),
            (
                r#",{"kind":"packager","name":"files","version":"1","impl":"builtin"}"#,
                "duplicate",
            ),
            (
                r#",{"kind":"packager","name":"p","version":"1","impl":"scorers/p"}"#,
                "packager not builtin",
            ),
            (
                r#",{"kind":"scorer","name":"x","version":"1","impl":"../x"}"#,
                "impl outside",
            ),
            (
                r#",{"kind":"scorer","name":"x","version":"1","impl":"scorers/x","accepts":["zip"]}"#,
                "unknown packager",
            ),
            (
                r#",{"kind":"scorer","name":"x","version":"1","impl":"scorers/x","interactive":true}"#,
                "interactive scorer",
            ),
            (
                r#",{"kind":"runner","name":"r","version":"1","impl":"builtin","model":true,"runs_taskset_code":false}"#,
                "model on a producing runner",
            ),
        ] {
            assert!(Registry::parse(&base(bad)).is_err(), "{why}");
        }
    }

    #[test]
    fn plugin_manifest_rules() {
        let ok = br#"{"schema":1,"kind":"scorer","name":"kw","version":"0.1","runs_taskset_code":false,"accepts":["files"]}"#;
        let m = PluginManifest::parse(ok).unwrap();
        let blob = crate::BlobRef {
            sha256: "a".repeat(64),
            key_id: "1ffa702796eb5ee8".into(),
        };
        let u = m.pin("u-0123456789abcdef", blob.clone());
        u.check().unwrap();
        assert!(u.as_plugin().is_user() && u.as_plugin().user);
        assert!(m.pin("kw", blob).check().is_err());
        for bad in [
            r#"{"schema":1,"kind":"runner","name":"kw","version":"1"}"#,
            r#"{"schema":1,"kind":"scorer","name":"KW","version":"1"}"#,
            r#"{"schema":1,"kind":"scorer","name":"kw","version":"1","model":true}"#,
            r#"{"schema":1,"kind":"scorer","name":"kw","version":"1","accepts":["zip"]}"#,
            r#"{"schema":1,"kind":"scorer","name":"kw","version":"1","extra":1}"#,
            r#"{"schema":2,"kind":"scorer","name":"kw","version":"1"}"#,
        ] {
            assert!(PluginManifest::parse(bad.as_bytes()).is_err(), "{bad}");
        }
    }
}
