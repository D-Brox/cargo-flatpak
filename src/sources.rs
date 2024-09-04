use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use toml::{map::Map, Value};
use url::Url;
use crate::{CARGO_CRATES, COMMIT_LEN, CRATES_IO, GIT_CACHE, VENDORED_SOURCES};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Archive {
    #[serde(rename = "archive-type")]
    pub archive_type: String,
    pub url: String,
    pub sha256: String,
    pub dest: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Inline {
    pub contents: String,
    pub dest: String,
    #[serde(rename = "dest-filename")]
    pub dest_filename: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Git {
    pub url: String,
    pub commit: String,
    pub dest: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Shell {
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum Source {
    #[serde(rename = "archive")]
    Archive(Archive),
    #[serde(rename = "inline")]
    Inline(Inline),
    #[serde(rename = "git")]
    Git(Git),
    #[serde(rename = "shell")]
    Shell(Shell),
}

#[derive(Debug, serde::Deserialize)]
pub struct LockFile {
    #[allow(dead_code)]
    pub version: u32,
    pub package: Vec<Package>,
}

#[derive(Debug, serde::Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub checksum: Option<String>,
    #[allow(dead_code)]
    pub dependencies: Option<Vec<String>>,
}

/// Converts a string to a Cargo Canonical URL,
/// as per https://github.com/rust-lang/cargo/blob/rust-1.82.0/src/cargo/util/canonical_url.rs
/// Since it comes from Cargo.lock, it's already partially formatted, we can skip some steps
fn parse_url(url: &str) -> Result<(Url,HashMap<String,String>), url::ParseError> {
    // Converts a string to a Cargo Canonical URL
    let url = url.replace("git+https://", "https://");
    let mut parsed_url = Url::parse(&url)?;
    
    let mut vendored_sources = HashMap::new();

    let query = parsed_url.query_pairs().collect::<HashMap<_,_>>();
    if let Some(rev) = query.get("rev"){
        vendored_sources.insert("rev".to_string(), rev.to_string());
    } else if let Some(tag) = query.get("tag") {
        vendored_sources.insert("tag".to_string(), tag.to_string());
    } else if let Some(branch) = query.get("branch") {
        vendored_sources.insert("branch".to_string(), branch.to_string());
    }

    // It seems cargo drops query and fragment
    parsed_url.set_query(None);
    parsed_url.set_fragment(None);

    // Remove trailing slashes
    let path = parsed_url.path().trim_end_matches('/').trim_end_matches(".git").to_owned();
    parsed_url.set_path(&path);

    vendored_sources.insert("git".to_string(),parsed_url.to_string());
    vendored_sources.insert("replace-with".to_string(),VENDORED_SOURCES.to_string());
    Ok((parsed_url,vendored_sources))
}

fn git_repo_name(git_url: &str, commit: &str) -> Result<String, url::ParseError> {
    let (canonical,_) = parse_url(git_url)?;
    let path = canonical.path();
    let name: &str = path.split('/').last().unwrap_or("");
    Ok(format!("{}-{}", name, &commit[..COMMIT_LEN]))
}

#[derive(serde::Serialize)]
struct GitPackage {
    path: PathBuf,
    package: toml::Value,
    workspace: Option<toml::Value>,
}

impl GitPackage {
    pub fn normalized(&self) -> toml::Value {
        let mut package = self.package.clone();
        if let Some(workspace) = &self.workspace {
            for (section_key, section) in package.as_table_mut().unwrap().iter_mut() {
                if let toml::Value::Table(section_map) = section {
                    let mut keys_to_replace = Vec::new();
                    for (key, value) in section_map.iter() {
                        if let toml::Value::Table(value_map) = value {
                            if value_map.contains_key("workspace") {
                                keys_to_replace.push(key.clone());
                            }
                        }
                    }
                    if let Some(workspace_section) =
                        workspace.get(section_key).and_then(toml::Value::as_table)
                    {
                        for key in keys_to_replace {
                            if let Some(workspace_value) = workspace_section.get(&key) {
                                section_map.insert(key, workspace_value.clone());
                            }
                        }
                    }
                }
            }
        }
        package
    }
}

type GitPackagesType = HashMap<String, GitPackage>;

#[allow(clippy::only_used_in_recursion)]
fn get_cargo_toml_packages(
    root_toml: toml::Value,
    root_dir: impl AsRef<Path>,
) -> anyhow::Result<GitPackagesType> {
    let root_dir = root_dir.as_ref();
    assert!(root_toml.get("package").is_some() || root_toml.get("workspace").is_some());
    let mut packages: GitPackagesType = HashMap::new();

    fn get_dep_packages<'a>(
        entry: &'a toml::Value,
        toml_dir: &'a Path,
        workspace: Option<&'a toml::Value>,
        packages: &'a mut GitPackagesType,
        root_dir: &'a Path,
    ) -> anyhow::Result<()> {
            // TODO: Use proper serde deserializer
            if let Some(dependencies) = entry.get("dependencies").and_then(|d| d.as_table()) {
                for (dep_name, dep) in dependencies {
                    let mut dep_name = dep_name.to_string();
                    if let Some(package) = dep.get("package").and_then(|p| p.as_str()) {
                        dep_name = package.to_string();
                    }
                    if dep.get("path").is_none() {
                        continue;
                    }
                    if packages.contains_key(&dep_name) {
                        continue;
                    }
                    let dep_dir = pathdiff::diff_paths(
                        toml_dir.join(dep.get("path").and_then(|p| p.as_str()).unwrap()),
                        Path::new("."),
                    )
                    .unwrap();
                    log::debug!("Loading dependency {} from {:?}", dep_name, dep_dir);
                    let dep_toml: toml::Value = toml::from_str(
                        &std::fs::read_to_string(dep_dir.join("Cargo.toml")).unwrap(),
                    )?;
                    assert_eq!(
                        dep_toml
                            .get("package")
                            .and_then(|p| p.get("name"))
                            .and_then(|n| n.as_str()),
                        Some(dep_name.as_str())
                    );

                    get_dep_packages(&dep_toml, &dep_dir, workspace, packages, root_dir)?;

                    packages.insert(
                        dep_name,
                        GitPackage {
                            path: dep_dir,
                            package: dep.clone(),
                            workspace: workspace.cloned(),
                        },
                    );
                }
            }
            if let Some(targets) = entry.get("target").and_then(|t| t.as_table()) {
                for target in targets.values() {
                    get_dep_packages(target, toml_dir, workspace, packages, root_dir)?;
                }
            }

            Ok(())
    }

    if let Some(package) = root_toml.get("package") {
        get_dep_packages(&root_toml, root_dir, None, &mut packages, root_dir)?;
        packages.insert(
            package
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap()
                .to_string(),
            GitPackage {
                path: root_dir.to_path_buf(),
                package: root_toml.clone(),
                workspace: None,
            },
        );
    }

    if let Some(workspace) = root_toml.get("workspace") {
        if let Some(members) = workspace.get("members").and_then(|m| m.as_array()) {
            for member in members.iter().filter_map(|m| m.as_str()) {
                for subpkg_toml in glob::glob(&format!(
                    "{}/{}/Cargo.toml",
                    root_dir.to_string_lossy(),
                    member
                ))? {
                    match subpkg_toml {
                        Ok(path) => {
                            let subpkg = path.parent().unwrap();
                            log::debug!("Loading workspace member {:?} in {:?}", path, root_dir);
                            let pkg_toml: toml::Value =
                                toml::from_str(&std::fs::read_to_string(&path).unwrap())?;
                            get_dep_packages(
                                &pkg_toml,
                                subpkg,
                                Some(workspace),
                                &mut packages,
                                root_dir,
                            )?;
                            packages.insert(
                                pkg_toml
                                    .get("package")
                                    .and_then(|p| p.get("name"))
                                    .and_then(|n| n.as_str())
                                    .unwrap()
                                    .to_string(),
                                GitPackage {
                                    path: subpkg.to_path_buf(),
                                    package: pkg_toml,
                                    workspace: Some(workspace.clone()),
                                },
                            );
                        }
                        Err(e) => eprintln!("{:?}", e),
                    }
                }
            }
        }
    }

    Ok(packages)
}

fn load_toml(src: &str) -> toml::Value {
    toml::from_str(src).unwrap()
}

fn get_git_package_sources(package: &Package, manifest: &str) -> (Vec<Source>,Map<String,Value>) {
    let name = package.name.clone();
    let source = package.source.clone().unwrap();

    let commit = Url::parse(&source)
        .unwrap()
        .fragment()
        .map(|f| f.to_string())
        .expect("The commit needs to be indicated in the fragment part");

    let (canonical,vendored) = parse_url(&source).unwrap();

    let repo_url = canonical.to_string();

    let toml_content = std::fs::read_to_string(manifest).unwrap();
    
    let packages = get_cargo_toml_packages(load_toml(&toml_content), manifest.trim_end_matches("/Cargo.toml")).expect("failed to get packages from manifest");

    let dest = format!("{name}-{}", &commit[..COMMIT_LEN]);

    let git_pkg = &packages.get(&name).unwrap();
    let pkg_repo_dir = format!(
        "{GIT_CACHE}/{}/{}",
        git_repo_name(&repo_url, &commit).unwrap(),
        git_pkg.path.to_string_lossy(),
    );

    let shell = Source::Shell(Shell {
        commands: vec![format!(
            r#"cp -r --reflink=auto "{pkg_repo_dir}" "{CARGO_CRATES}/{name}""#
        )],
    });

    let cargo_toml = Source::Inline(Inline {
        contents: toml::to_string(&git_pkg.normalized()).unwrap(),
        dest: format!("{CARGO_CRATES}/{name}"),
        dest_filename: "Cargo.toml".to_string(),
    });

    let cargo_checksum = Source::Inline(Inline {
        contents: r#"{"package": null, "files": {}}"#.to_string(),
        dest: format!("{CARGO_CRATES}/{name}"),
        dest_filename: ".cargo-checksum.json".to_string(),
    });

    let git = Source::Git(Git {
        url: repo_url,
        commit,
        dest,
    });

    let mut c = Map::new();
    c.insert(canonical.to_string().to_string(), vendored.into());

    (vec![git, shell, cargo_toml, cargo_checksum],c)
}

pub fn get_package_sources(
    package: &Package,
    manifest: &str
) -> Option<(Vec<Source>, Map<String, toml::Value>)> {
    let name = &package.name;
    let version = &package.version;

    if let Some(source) = package.source.as_ref() {
        if source.starts_with("git+") {
            let (source,c) = get_git_package_sources(package,manifest);
            return Some((source, c));
        }

        if let Some(checksum) = package.checksum.as_ref() {
            let archive = Source::Archive(Archive {
                archive_type: "tar-gzip".into(),
                url: format!("{CRATES_IO}/{name}/{name}-{version}.crate"),
                sha256: checksum.into(),
                dest: format!("{CARGO_CRATES}/{name}-{version}"),
            });

            let inline = Source::Inline(Inline {
                contents: format!(r#"{{"package": "{checksum}", "files": {{}}}}"#),
                dest: format!("{CARGO_CRATES}/{name}-{version}"),
                dest_filename: ".cargo-checksum.json".into(),
            });

            let crate_sources = vec![archive, inline];

            let mut c = Map::new();
            c.insert("crates-io".into(), {
                let mut obj = Map::new();
                obj.insert("replace-with".into(), VENDORED_SOURCES.into());
                obj.into()
            });

            return Some((crate_sources, c));
        }
    }

    None
}

#[test]
fn lock_file() {
    let src = std::fs::read_to_string("./Cargo.lock").unwrap();

    let file: LockFile = toml::from_str(&src).unwrap();

    dbg!(file);
}

#[test]
fn source() {
    let src = Source::Inline(Inline {
        contents: "a".into(),
        dest: "a".into(),
        dest_filename: "a".into(),
    });

    println!("{}", serde_json::to_string_pretty(&src).unwrap());
}
