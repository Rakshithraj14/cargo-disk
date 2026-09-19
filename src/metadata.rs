use crate::json::{self, Json};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Metadata {
    pub target_dir: PathBuf,
    // Where deps/, build/ and incremental/ go. Same as target_dir unless
    // `build.build-dir` is set, in which case target_dir holds little more
    // than the final binaries.
    pub build_dir: PathBuf,
    pub packages: Vec<Package>,
    pub members: Vec<String>,
    pub nodes: Vec<Node>,
}

pub struct Package {
    pub id: String,
    pub name: String,
    pub manifest_path: PathBuf,
    pub targets: Vec<Target>,
    pub deps: Vec<Dep>,
    pub ignore: Vec<String>,
}

pub struct Target {
    pub name: String,
    pub is_lib: bool,
}

pub struct Dep {
    pub name: String,
    pub rename: Option<String>,
    pub kind: Kind,
    pub target: Option<String>,
}

pub struct Node {
    pub id: String,
    pub deps: Vec<NodeDep>,
}

pub struct NodeDep {
    /// The identifier code uses: the rename, or the dependency's lib name.
    pub name: String,
    pub pkg: String,
    pub kinds: Vec<Kind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Normal,
    Dev,
    Build,
}

impl Kind {
    fn from_json(value: Option<&Json>) -> Kind {
        match value.and_then(Json::str) {
            Some("dev") => Kind::Dev,
            Some("build") => Kind::Build,
            _ => Kind::Normal,
        }
    }
}

pub enum Mode {
    /// Local packages only. Never needs the network or dependency sources.
    NoDeps,
    /// Full graph, but only from what is already downloaded.
    Offline,
    /// Full graph, downloading sources if needed, never rewriting Cargo.lock.
    Online,
}

pub fn load(root: &Path, mode: Mode) -> Result<Metadata, String> {
    let flags: &[&str] = match mode {
        Mode::NoDeps => &["--no-deps", "--offline"],
        Mode::Offline => &["--frozen"],
        Mode::Online => &["--locked"],
    };
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .args(flags)
        .current_dir(root)
        .output()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err
            .lines()
            .next()
            .unwrap_or("cargo metadata failed")
            .to_string());
    }
    let text = String::from_utf8(out.stdout).map_err(|_| "cargo metadata: not UTF-8")?;
    let doc = json::parse(&text).ok_or("cargo metadata: unreadable output")?;
    from_json(&doc).ok_or_else(|| "cargo metadata: unexpected format".to_string())
}

pub fn from_json(doc: &Json) -> Option<Metadata> {
    let path = |key: &str| doc.get(key).and_then(Json::str).map(PathBuf::from);
    let target_dir = path("target_directory")?;
    let build_dir = path("build_directory").unwrap_or_else(|| target_dir.clone());
    let strs = |v: Option<&Json>| -> Vec<String> {
        v.map(Json::arr)
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.str().map(String::from))
            .collect()
    };

    let packages =
        doc.get("packages")?
            .arr()
            .iter()
            .filter_map(|p| {
                Some(Package {
                    id: p.get("id")?.str()?.to_string(),
                    name: p.get("name")?.str()?.to_string(),
                    manifest_path: PathBuf::from(p.get("manifest_path")?.str()?),
                    targets: p
                        .get("targets")?
                        .arr()
                        .iter()
                        .filter_map(|t| {
                            Some(Target {
                                name: t.get("name")?.str()?.to_string(),
                                is_lib: t.get("kind")?.arr().iter().any(|k| {
                                    matches!(k.str(), Some("lib" | "rlib" | "proc-macro"))
                                }),
                            })
                        })
                        .collect(),
                    deps: p
                        .get("dependencies")?
                        .arr()
                        .iter()
                        .filter_map(|d| {
                            Some(Dep {
                                name: d.get("name")?.str()?.to_string(),
                                rename: d.get("rename").and_then(Json::str).map(String::from),
                                kind: Kind::from_json(d.get("kind")),
                                target: d.get("target").and_then(Json::str).map(String::from),
                            })
                        })
                        .collect(),
                    ignore: strs(
                        p.get("metadata")
                            .and_then(|m| m.get("cargo-disk"))
                            .and_then(|m| m.get("ignore")),
                    ),
                })
            })
            .collect();

    let nodes = doc
        .get("resolve")
        .and_then(|r| r.get("nodes"))
        .map(Json::arr)
        .unwrap_or_default()
        .iter()
        .filter_map(|n| {
            Some(Node {
                id: n.get("id")?.str()?.to_string(),
                deps: n
                    .get("deps")?
                    .arr()
                    .iter()
                    .filter_map(|d| {
                        Some(NodeDep {
                            name: d.get("name")?.str()?.to_string(),
                            pkg: d.get("pkg")?.str()?.to_string(),
                            kinds: d
                                .get("dep_kinds")?
                                .arr()
                                .iter()
                                .map(|k| Kind::from_json(k.get("kind")))
                                .collect(),
                        })
                    })
                    .collect(),
            })
        })
        .collect();

    Some(Metadata {
        target_dir,
        build_dir,
        packages,
        members: strs(doc.get("workspace_members")),
        nodes,
    })
}

impl Metadata {
    pub fn package(&self, id: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.id == id)
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Every name an artifact in target/ could legitimately carry: package
    /// names and target names (they differ: md-5 builds md5), normalised the
    /// way rustc names files.
    pub fn artifact_names(&self) -> impl Iterator<Item = String> + '_ {
        self.packages
            .iter()
            .flat_map(|p| std::iter::once(&p.name).chain(p.targets.iter().map(|t| &t.name)))
            .chain(
                self.nodes
                    .iter()
                    .flat_map(|n| n.deps.iter().map(|d| &d.name)),
            )
            .map(|n| n.replace('-', "_"))
    }
}

impl Package {
    /// Names this package's artifacts appear under in target/: its lib
    /// target, and its package name (build-script directories use that).
    pub fn artifact_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .targets
            .iter()
            .filter(|t| t.is_lib)
            .map(|t| t.name.replace('-', "_"))
            .chain(std::iter::once(self.name.replace('-', "_")))
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
        "target_directory": "/p/out", "build_directory": "/p/bld",
        "workspace_members": ["path+file:///p#app@0.1.0"],
        "packages": [
          {"id": "path+file:///p#app@0.1.0", "name": "app",
           "manifest_path": "/p/Cargo.toml",
           "targets": [{"kind": ["bin"], "name": "app"}],
           "dependencies": [
             {"name": "md-5", "rename": "hashing", "kind": null, "target": null},
             {"name": "memchr", "rename": null, "kind": "dev", "target": null},
             {"name": "winapi", "rename": null, "kind": null, "target": "cfg(windows)"}],
           "metadata": {"cargo-disk": {"ignore": ["winapi"]}}},
          {"id": "reg#md-5@0.10.6", "name": "md-5", "manifest_path": "/r/Cargo.toml",
           "targets": [{"kind": ["lib"], "name": "md5"}],
           "dependencies": [], "metadata": null}],
        "resolve": {"nodes": [
          {"id": "path+file:///p#app@0.1.0", "deps": [
            {"name": "hashing", "pkg": "reg#md-5@0.10.6",
             "dep_kinds": [{"kind": null, "target": null}]}]}]}
    }"#;

    #[test]
    fn reads_the_fields_the_tool_relies_on() {
        let meta = from_json(&json::parse(DOC).unwrap()).unwrap();
        assert_eq!(meta.target_dir, PathBuf::from("/p/out"));
        assert_eq!(meta.build_dir, PathBuf::from("/p/bld"));
        assert_eq!(meta.members, vec!["path+file:///p#app@0.1.0"]);

        let app = meta.package("path+file:///p#app@0.1.0").unwrap();
        assert_eq!(app.ignore, vec!["winapi"]);
        assert_eq!(app.deps[0].rename.as_deref(), Some("hashing"));
        assert_eq!(app.deps[1].kind, Kind::Dev);
        assert_eq!(app.deps[2].target.as_deref(), Some("cfg(windows)"));

        let md5 = meta.package("reg#md-5@0.10.6").unwrap();
        assert_eq!(md5.artifact_names(), vec!["md5", "md_5"]);

        let dep = &meta.node("path+file:///p#app@0.1.0").unwrap().deps[0];
        assert_eq!(
            (dep.name.as_str(), dep.kinds.as_slice()),
            ("hashing", &[Kind::Normal][..])
        );

        let names: Vec<String> = meta.artifact_names().collect();
        assert!(
            names.contains(&"md5".to_string()),
            "lib name, not just package name"
        );
    }

    #[test]
    fn older_cargo_without_build_directory_falls_back_to_target() {
        let doc = json::parse(r#"{"target_directory":"/t","packages":[]}"#).unwrap();
        let meta = from_json(&doc).unwrap();
        assert_eq!(meta.build_dir, PathBuf::from("/t"));
        assert!(meta.nodes.is_empty());
    }
}
