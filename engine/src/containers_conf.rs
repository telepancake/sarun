//! /etc/containers/registries.conf support: short-name aliases,
//! unqualified-search-registries, and per-registry location/mirror/blocked
//! remapping — the subset of containers-registries.conf(5) that decides
//! WHERE an image reference is actually pulled from. Sites use these to
//! force pulls through a local mirror (policy, or to dodge public-registry
//! rate limits / IP blocks); sarun previously ignored them and always went
//! to docker.io, which is exactly wrong on such hosts.
//!
//! Parsing is deliberately lenient: a missing file yields the built-in
//! default (search = docker.io), a malformed drop-in is skipped, and a
//! reference that is already fully qualified with no matching remap passes
//! through untouched — behavior identical to before this module existed.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// One `[[registry]]` table: remap/block pulls whose reference starts with
/// `prefix` (host[:port][/namespace…]; defaults to `location` when absent).
#[derive(Debug, Clone, Default)]
pub struct RegistryEntry {
    pub prefix: String,
    pub location: String,
    pub blocked: bool,
    /// `[[registry.mirror]]` locations, tried in order BEFORE the primary.
    pub mirrors: Vec<String>,
}

/// The merged view of registries.conf + its drop-in directory.
#[derive(Debug, Clone, Default)]
pub struct ContainersConf {
    /// `unqualified-search-registries`, in order. Empty = docker.io fallback.
    pub search: Vec<String>,
    /// `[aliases]` short-name → fully-qualified image (no tag). BTreeMap so
    /// the UI lists them in a stable order.
    pub aliases: BTreeMap<String, String>,
    pub registries: Vec<RegistryEntry>,
    /// Files that actually contributed config (for UI display).
    pub sources: Vec<PathBuf>,
}

/// One concrete pull attempt for a reference, plus a human-readable note of
/// how the candidate came to be ("short-name alias", "mirror of …", …).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub reference: String,
    pub via: String,
}

/// resolve() output: candidates in pull order; `blocked` carries a message
/// when policy blocks the (only) source outright.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub candidates: Vec<Candidate>,
    pub blocked: Option<String>,
}

impl ContainersConf {
    /// Load the host configuration: base file (first hit wins) then `.conf`
    /// drop-ins (sorted; later files override aliases/search, append
    /// registries) — the same precedence podman documents.
    pub fn load() -> ContainersConf {
        let mut files: Vec<PathBuf> = Vec::new();
        let base = std::env::var("CONTAINERS_REGISTRIES_CONF").ok()
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .or_else(|| {
                user_config_dir()
                    .map(|d| d.join("containers/registries.conf"))
                    .filter(|p| p.is_file())
            })
            .or_else(|| {
                ["/etc/containers/registries.conf",
                 "/usr/share/containers/registries.conf"]
                    .iter().map(PathBuf::from).find(|p| p.is_file())
            });
        files.extend(base);
        let mut dropin_dirs = vec![PathBuf::from("/etc/containers/registries.conf.d")];
        if let Some(u) = user_config_dir() {
            dropin_dirs.push(u.join("containers/registries.conf.d"));
        }
        for d in dropin_dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "conf"))
                .collect();
            names.sort();
            files.extend(names);
        }
        let mut conf = ContainersConf::default();
        for f in files {
            if let Ok(s) = std::fs::read_to_string(&f) {
                if conf.merge_toml(&s) {
                    conf.sources.push(f);
                }
            }
        }
        conf
    }

    /// Merge one TOML document into self. Returns false when the document
    /// doesn't parse (skipped, matching podman's tolerance of stray files).
    pub fn merge_toml(&mut self, text: &str) -> bool {
        let Ok(v) = text.parse::<toml::Value>() else { return false };
        let Some(t) = v.as_table() else { return false };
        if let Some(arr) = t.get("unqualified-search-registries")
            .and_then(|v| v.as_array())
        {
            self.search = arr.iter()
                .filter_map(|s| s.as_str().map(str::to_string)).collect();
        }
        if let Some(al) = t.get("aliases").and_then(|v| v.as_table()) {
            for (k, val) in al {
                if let Some(s) = val.as_str() {
                    self.aliases.insert(k.clone(), s.to_string());
                }
            }
        }
        if let Some(regs) = t.get("registry").and_then(|v| v.as_array()) {
            for r in regs {
                let Some(rt) = r.as_table() else { continue };
                let location = rt.get("location").and_then(|v| v.as_str())
                    .unwrap_or("").to_string();
                let prefix = rt.get("prefix").and_then(|v| v.as_str())
                    .unwrap_or(&location).to_string();
                if prefix.is_empty() { continue; }
                let blocked = rt.get("blocked").and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mirrors = rt.get("mirror").and_then(|v| v.as_array())
                    .map(|ms| ms.iter()
                        .filter_map(|m| m.as_table())
                        .filter_map(|m| m.get("location"))
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect())
                    .unwrap_or_default();
                self.registries.push(RegistryEntry {
                    prefix, location, blocked, mirrors,
                });
            }
        }
        true
    }

    /// Turn a user-typed reference into the ordered pull candidates the
    /// host config dictates. Fully-qualified refs pass through (subject to
    /// mirror/blocked remap); unqualified refs go through aliases, then the
    /// search list, then the docker.io/library fallback.
    pub fn resolve(&self, reference: &str) -> Resolved {
        let (name, suffix) = split_ref(reference);
        // Fully-qualified bases the remap pass runs over, each with a note.
        let mut bases: Vec<(String, String)> = Vec::new();
        if is_qualified(name) {
            bases.push((name.to_string(), String::new()));
        } else if let Some(target) = self.aliases.get(name) {
            bases.push((target.clone(), "short-name alias".into()));
        } else {
            let fallback = ["docker.io".to_string()];
            let search: &[String] = if self.search.is_empty() {
                &fallback
            } else {
                &self.search
            };
            for reg in search {
                bases.push((qualify(reg, name), format!("search: {reg}")));
            }
        }
        let mut out = Resolved::default();
        for (base, via) in bases {
            let full = format!("{base}{suffix}");
            match self.remap(&full) {
                Remap::Blocked(prefix) => {
                    out.blocked.get_or_insert(format!(
                        "'{full}' is blocked by registries.conf \
                         ([[registry]] prefix \"{prefix}\")"));
                }
                Remap::Pass => out.push(full, via.clone()),
                Remap::To(cands) => for (r, note) in cands {
                    let via = if via.is_empty() { note }
                              else if note.is_empty() { via.clone() }
                              else { format!("{via} · {note}") };
                    out.push(r, via);
                }
            }
        }
        // A block only matters when it left us with nothing to try.
        if !out.candidates.is_empty() { out.blocked = None; }
        out
    }

    fn remap(&self, full: &str) -> Remap {
        // Longest matching prefix wins (spec: more specific wins).
        let hit = self.registries.iter()
            .filter(|e| prefix_matches(&e.prefix, full))
            .max_by_key(|e| e.prefix.len());
        let Some(e) = hit else { return Remap::Pass };
        if e.blocked { return Remap::Blocked(e.prefix.clone()); }
        let rest = &full[e.prefix.len()..];
        let mut cands: Vec<(String, String)> = e.mirrors.iter()
            .map(|m| (format!("{m}{rest}"), format!("mirror of {}", e.prefix)))
            .collect();
        let primary = if e.location.is_empty() { full.to_string() }
                      else { format!("{}{rest}", e.location) };
        let note = if e.location.is_empty() || e.location == e.prefix {
            String::new()
        } else {
            format!("remapped from {}", e.prefix)
        };
        cands.push((primary, note));
        Remap::To(cands)
    }
}

enum Remap {
    Pass,
    Blocked(String),
    To(Vec<(String, String)>),
}

impl Resolved {
    fn push(&mut self, reference: String, via: String) {
        if self.candidates.iter().any(|c| c.reference == reference) { return; }
        self.candidates.push(Candidate { reference, via });
    }
}

fn user_config_dir() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok()
            .map(|h| Path::new(&h).join(".config")))
}

/// Split `name[:tag][@digest]` into (name, ":tag@digest" suffix). The tag
/// colon is the last ':' AFTER the last '/', so `host:5000/img` stays whole.
fn split_ref(reference: &str) -> (&str, &str) {
    let (body, digest_at) = match reference.find('@') {
        Some(i) => (&reference[..i], i),
        None => (reference, reference.len()),
    };
    let slash = body.rfind('/').map(|i| i + 1).unwrap_or(0);
    if let Some(c) = body[slash..].rfind(':') {
        let i = slash + c;
        (&reference[..i], &reference[i..])
    } else {
        (&reference[..digest_at], &reference[digest_at..])
    }
}

/// A reference is fully qualified when its first path component names a
/// host: contains '.' or ':', or is exactly "localhost".
fn is_qualified(name: &str) -> bool {
    let first = name.split('/').next().unwrap_or("");
    name.contains('/') &&
        (first.contains('.') || first.contains(':') || first == "localhost")
}

/// Prepend a search registry; docker.io single-component names get the
/// implicit `library/` namespace docker hub requires.
fn qualify(registry: &str, name: &str) -> String {
    if (registry == "docker.io" || registry.ends_with(".docker.io"))
        && !name.contains('/')
    {
        format!("{registry}/library/{name}")
    } else {
        format!("{registry}/{name}")
    }
}

/// `prefix` matches when it equals `full` or is a parent component-wise
/// (`quay.io/foo` matches `quay.io/foo/bar` and `quay.io/foo:tag`, not
/// `quay.io/foobar`).
fn prefix_matches(prefix: &str, full: &str) -> bool {
    full.strip_prefix(prefix).is_some_and(|rest| {
        rest.is_empty() || rest.starts_with('/')
            || rest.starts_with(':') || rest.starts_with('@')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf(text: &str) -> ContainersConf {
        let mut c = ContainersConf::default();
        assert!(c.merge_toml(text), "test toml must parse");
        c
    }

    #[test]
    fn default_docker_io_fallback() {
        let c = ContainersConf::default();
        let r = c.resolve("ubuntu:24.04");
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].reference, "docker.io/library/ubuntu:24.04");
    }

    #[test]
    fn qualified_passthrough() {
        let c = ContainersConf::default();
        let r = c.resolve("ghcr.io/foo/bar:tag");
        assert_eq!(r.candidates[0].reference, "ghcr.io/foo/bar:tag");
        let r = c.resolve("localhost/img");
        assert_eq!(r.candidates[0].reference, "localhost/img");
        // host:port counts as qualified.
        let r = c.resolve("reg.example:5000/img:v1");
        assert_eq!(r.candidates[0].reference, "reg.example:5000/img:v1");
    }

    #[test]
    fn alias_wins_over_search() {
        let c = conf(r#"
            unqualified-search-registries = ["quay.io", "docker.io"]
            [aliases]
            "ubuntu" = "docker.io/library/ubuntu"
            "fedora" = "registry.fedoraproject.org/fedora"
        "#);
        let r = c.resolve("fedora:41");
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.candidates[0].reference,
                   "registry.fedoraproject.org/fedora:41");
        assert_eq!(r.candidates[0].via, "short-name alias");
    }

    #[test]
    fn search_list_order_and_library() {
        let c = conf(r#"unqualified-search-registries = ["quay.io", "docker.io"]"#);
        let r = c.resolve("something:1");
        assert_eq!(r.candidates[0].reference, "quay.io/something:1");
        assert_eq!(r.candidates[1].reference, "docker.io/library/something:1");
    }

    #[test]
    fn mirror_before_primary() {
        let c = conf(r#"
            [[registry]]
            prefix = "docker.io"
            location = "docker.io"
            [[registry.mirror]]
            location = "mirror.corp.example/dockerhub"
        "#);
        let r = c.resolve("ubuntu:24.04");
        assert_eq!(r.candidates[0].reference,
                   "mirror.corp.example/dockerhub/library/ubuntu:24.04");
        assert!(r.candidates[0].via.contains("mirror of docker.io"));
        assert_eq!(r.candidates[1].reference, "docker.io/library/ubuntu:24.04");
    }

    #[test]
    fn location_remap_and_longest_prefix() {
        let c = conf(r#"
            [[registry]]
            prefix = "quay.io"
            location = "internal.example/quay"
            [[registry]]
            prefix = "quay.io/special"
            location = "special.example"
        "#);
        let r = c.resolve("quay.io/foo/bar:1");
        assert_eq!(r.candidates[0].reference, "internal.example/quay/foo/bar:1");
        let r = c.resolve("quay.io/special/x");
        assert_eq!(r.candidates[0].reference, "special.example/x");
        // component-wise: quay.io/specialx must NOT match quay.io/special
        let r = c.resolve("quay.io/specialx/x");
        assert_eq!(r.candidates[0].reference, "internal.example/quay/specialx/x");
    }

    #[test]
    fn blocked_registry_fails_closed() {
        let c = conf(r#"
            unqualified-search-registries = ["docker.io"]
            [[registry]]
            prefix = "docker.io"
            blocked = true
        "#);
        let r = c.resolve("ubuntu");
        assert!(r.candidates.is_empty());
        assert!(r.blocked.unwrap().contains("blocked"));
    }

    #[test]
    fn digest_refs_keep_digest() {
        let c = ContainersConf::default();
        let r = c.resolve("ubuntu@sha256:abcd");
        assert_eq!(r.candidates[0].reference,
                   "docker.io/library/ubuntu@sha256:abcd");
        let (n, s) = split_ref("host.io/img:tag@sha256:ff");
        assert_eq!(n, "host.io/img");
        assert_eq!(s, ":tag@sha256:ff");
    }

    #[test]
    fn dropin_merge_overrides_aliases_appends_registries() {
        let mut c = conf(r#"
            unqualified-search-registries = ["docker.io"]
            [aliases]
            "app" = "docker.io/library/app"
        "#);
        c.merge_toml(r#"
            [aliases]
            "app" = "quay.io/corp/app"
            [[registry]]
            prefix = "quay.io"
            location = "quay.io"
        "#);
        assert_eq!(c.aliases["app"], "quay.io/corp/app");
        assert_eq!(c.search, vec!["docker.io"]);
        assert_eq!(c.registries.len(), 1);
        assert!(!c.merge_toml("not [ valid toml"), "garbage must be skipped");
    }
}
