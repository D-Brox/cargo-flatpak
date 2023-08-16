use std::{
    collections::HashMap,
    env,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
};

use clap::Parser;
use sha2::{Digest, Sha256};
use url::Url;

use crate::lock_file::LockFile;

mod lock_file;
mod manifest;

const CRATES_IO: &str = "https://static.crates.io/crates";
const CARGO_HOME: &str = "cargo";
const CARGO_CRATES: &str = "cargo/vendor";
const VENDORED_SOURCES: &str = "vendored-sources";
const GIT_CACHE: &str = "flatpak-cargo/git";
const COMMIT_LEN: usize = 7;

/// Simple program to greet a person
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    cargo_lock: String,

    /// Name of the person to greet
    #[clap(short, long, default_value = "generated-sources.json")]
    output: String,
}

#[derive(Debug, Parser)]
#[clap(bin_name = "cargo")]
enum Command {
    Flatpack(Args),
}

/// Converts a string to a Cargo Canonical URL,
/// as per https://github.com/rust-lang/cargo/blob/35c55a93200c84a4de4627f1770f76a8ad268a39/src/cargo/util/canonical_url.rs#L19
fn canonical_url(url: &str) -> Result<Url, url::ParseError> {
    // Converts a string to a Cargo Canonical URL
    let url = url.replace("git+https://", "https://");
    let mut parsed_url = Url::parse(&url)?;

    // It seems cargo drops query and fragment
    parsed_url.set_query(None);
    parsed_url.set_fragment(None);

    // Remove trailing slashes
    let path = parsed_url.path().trim_end_matches('/').to_owned();
    parsed_url.set_path(&path);

    if parsed_url.domain() == Some("github.com") {
        parsed_url.set_scheme("https").unwrap();
        let path = parsed_url.path().to_lowercase();
        parsed_url.set_path(&path);
    }

    if parsed_url.path().ends_with(".git") {
        let path = parsed_url.path().trim_end_matches(".git").to_owned();
        parsed_url.set_path(&path);
    }

    Ok(parsed_url)
}

fn get_git_tarball(repo_url: &str, commit: &str) -> String {
    let url = canonical_url(repo_url).unwrap();
    let path = url.path_segments().unwrap().collect::<Vec<_>>();
    assert!(path.len() == 2);
    let (owner, repo) = (path[0], path[1]);
    let hostname = url.host_str().unwrap();
    match hostname {
        "github.com" => format!(
            "https://codeload.{}/{}-{}/tar.gz/{}",
            hostname, owner, repo, commit
        ),
        "bitbucket.org" => format!(
            "https://{}/{}/{}/get/{}.tar.gz",
            hostname, owner, repo, commit
        ),
        _ if hostname.split('.').next().unwrap() == "gitlab" => format!(
            "https://{}/{}/{}/-/archive/{}/{}-{}.tar.gz",
            hostname, owner, repo, commit, repo, commit
        ),
        _ => panic!("Don't know how to get tarball for {}", repo_url),
    }
}

async fn get_remote_sha256(url: &str) -> String {
    let mut sha256 = Sha256::new();
    let data = reqwest::get(url).await.unwrap().bytes().await.unwrap();
    sha256.update(data);
    format!("{:x}", sha256.finalize())
}

fn git_repo_name(git_url: &str, commit: &str) -> Result<String, url::ParseError> {
    let canonical = canonical_url(git_url)?;
    let path = canonical.path();
    let name: &str = path.split('/').last().unwrap_or("");
    Ok(format!("{}-{}", name, &commit[..COMMIT_LEN]))
}

fn fetch_git_repo(git_url: &str, commit: &str) -> io::Result<String> {
    let repo_dir = git_url.replace("://", "_").replace("/", "_");

    let cache_dir = env::var("XDG_CACHE_HOME")
        .unwrap_or_else(|_| env::var("HOME").unwrap_or_else(|_| String::from("~/.cache")));
    let cache_dir = shellexpand::tilde(&cache_dir).into_owned();

    let clone_dir = Path::new(&cache_dir).join("flatpak-cargo").join(repo_dir);

    use std::process::Command;
    if !clone_dir.join(".git").is_dir() {
        Command::new("git")
            .args(&["clone", "--depth=1", git_url, clone_dir.to_str().unwrap()])
            .status()?;
    }

    let rev_parse_output = Command::new("git")
        .args(&["rev-parse", "HEAD"])
        .current_dir(&clone_dir)
        .output()?;

    let head = String::from_utf8_lossy(&rev_parse_output.stdout)
        .trim()
        .to_string();

    if &head[..COMMIT_LEN] != &commit[..COMMIT_LEN] {
        Command::new("git")
            .args(&["fetch", "origin", commit])
            .current_dir(&clone_dir)
            .status()?;

        Command::new("git")
            .args(&["checkout", commit])
            .current_dir(&clone_dir)
            .status()?;
    }

    Ok(clone_dir.to_str().unwrap().to_string())
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

async fn get_cargo_toml_packages(
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
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        Box::pin(async move {
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

                    get_dep_packages(&dep_toml, &dep_dir, workspace, packages, root_dir).await?;

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
                    get_dep_packages(target, toml_dir, workspace, packages, root_dir).await?;
                }
            }

            Ok(())
        })
    }

    if let Some(package) = root_toml.get("package") {
        get_dep_packages(&root_toml, &root_dir, None, &mut packages, root_dir).await?;
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
                                &subpkg,
                                Some(workspace),
                                &mut packages,
                                root_dir,
                            )
                            .await?;
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

async fn get_git_repo_packages(
    git_url: &str,
    commit: &str,
) -> Result<GitPackagesType, Box<dyn std::error::Error>> {
    log::info!("Loading packages from {}", git_url);
    let git_repo_dir = fetch_git_repo(git_url, commit)?;
    let mut packages: GitPackagesType = HashMap::new();

    let cargo_toml_path = Path::new(&git_repo_dir).join("Cargo.toml");

    let current_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&git_repo_dir).unwrap();

    if cargo_toml_path.exists() {
        let toml_content = std::fs::read_to_string(&cargo_toml_path).unwrap();
        let packages_from_toml = get_cargo_toml_packages(load_toml(&toml_content), ".").await?;
        packages.extend(packages_from_toml);
    } else {
        let pattern = format!("{}/Cargo.toml", &git_repo_dir);
        for entry in glob::glob(&pattern)? {
            match entry {
                Ok(path) => {
                    let toml_content = std::fs::read_to_string(&path).unwrap();
                    let parent_dir = path.parent().unwrap();
                    let packages_from_toml = get_cargo_toml_packages(
                        load_toml(&toml_content),
                        parent_dir.to_string_lossy().as_ref(),
                    )
                    .await?;
                    packages.extend(packages_from_toml);
                }
                Err(e) => println!("{:?}", e),
            }
        }
    }

    std::env::set_current_dir(&current_dir).unwrap();

    assert!(
        !packages.is_empty(),
        "No packages found in {}",
        git_repo_dir
    );
    log::debug!(
        "Packages in {}:\n{}",
        git_url,
        serde_json::to_string_pretty(&packages)?
    );

    Ok(packages)
}

async fn get_git_package_sources(package: &lock_file::Package) -> Vec<manifest::Source> {
    let name = package.name.clone();
    let source = package.source.clone().unwrap();

    let commit = Url::parse(&source)
        .unwrap()
        .fragment()
        .map(|f| f.to_string())
        .expect("The commit needs to be indicated in the fragement part");

    let canonical = canonical_url(&source).unwrap();
    let repo_url = canonical.to_string();

    let packages = get_git_repo_packages(&repo_url, &commit).await.unwrap();

    let dest = format!("{name}-{}", &commit[..COMMIT_LEN]);

    let git_pkg = &packages.get(&name).unwrap();
    let pkg_repo_dir = format!(
        "{GIT_CACHE}/{}/{}",
        git_repo_name(&repo_url, &commit).unwrap(),
        git_pkg.path.to_string_lossy(),
    );
    dbg!(&pkg_repo_dir);

    let shell = manifest::Source::Shell(manifest::Shell {
        commands: vec![format!(
            r#"cp -r --reflink=auto "{pkg_repo_dir}" "{CARGO_CRATES}/{name}""#
        )],
    });

    let cargo_toml = manifest::Source::Inline(manifest::Inline {
        contents: toml::to_string(&git_pkg.normalized()).unwrap(),
        dest: format!("{CARGO_CRATES}/{name}"),
        dest_filename: "Cargo.toml".to_string(),
    });

    let cargo_checksum = manifest::Source::Inline(manifest::Inline {
        contents: r#"{"package": null, "files": {}}"#.to_string(),
        dest: format!("{CARGO_CRATES}/{name}"),
        dest_filename: ".cargo-checksum.json".to_string(),
    });

    let git = manifest::Source::Git(manifest::Git {
        url: repo_url,
        commit,
        dest,
    });

    // vec![git, shell, cargo_toml, cargo_checksum]
    vec![shell, cargo_toml, cargo_checksum]
}

async fn get_package_sources(
    package: &lock_file::Package,
) -> Option<(Vec<manifest::Source>, toml::map::Map<String, toml::Value>)> {
    let name = &package.name;
    let version = &package.version;

    if let Some(source) = package.source.as_ref() {
        if source.starts_with("git+") {
            let source = get_git_package_sources(package).await;

            let c = toml::map::Map::new();
            return Some((source, c));
        }

        if let Some(checksum) = package.checksum.as_ref() {
            let archive = manifest::Source::Archive(manifest::Archive {
                archive_type: "tar-gzip".into(),
                url: format!("{CRATES_IO}/{name}/{name}-{version}.crate"),
                sha256: checksum.into(),
                dest: format!("{CARGO_CRATES}/{name}-{version}"),
            });

            let inline = manifest::Source::Inline(manifest::Inline {
                contents: format!(r#"{{"package": "{checksum}", "files": {{}}}}"#),
                dest: format!("{CARGO_CRATES}/{name}-{version}"),
                dest_filename: ".cargo-checksum.json".into(),
            });

            let crate_sources = vec![archive, inline];

            let mut c = toml::map::Map::new();
            c.insert("crates-io".into(), {
                let mut obj = toml::map::Map::new();
                obj.insert("replace-with".into(), VENDORED_SOURCES.into());
                obj.into()
            });

            return Some((crate_sources, c));
        }
    }

    None
}

fn main() {
    let Command::Flatpack(args) = Command::parse();

    let cargo_lock = std::fs::read_to_string(&args.cargo_lock).unwrap();
    let cargo_lock: LockFile = toml::de::from_str(&cargo_lock).unwrap();

    let mut package_sources: Vec<manifest::Source> = Vec::new();

    let mut cargo_vendored_sources = toml::map::Map::new();
    cargo_vendored_sources.insert(VENDORED_SOURCES.into(), {
        let mut obj = toml::map::Map::new();
        obj.insert("directory".into(), CARGO_CRATES.into());
        obj.into()
    });

    for package in cargo_lock.package {
        pollster::block_on(async {
            if let Some((mut pkg_sources, cargo_vendored_entry)) =
                get_package_sources(&package).await
            {
                package_sources.append(&mut pkg_sources);

                for (key, value) in cargo_vendored_entry {
                    cargo_vendored_sources.insert(key, value);
                }
            }
        });
    }

    let mut sources = package_sources.clone();

    let cargo_vendored_sources = {
        let mut sources = toml::map::Map::new();
        sources.insert("source".into(), cargo_vendored_sources.into());
        let source = toml::to_string(&sources).unwrap();

        manifest::Source::Inline(manifest::Inline {
            contents: source,
            dest: CARGO_HOME.into(),
            dest_filename: "config".into(),
        })
    };

    sources.push(cargo_vendored_sources);

    println!("{}", serde_json::to_string_pretty(&sources).unwrap());
}
