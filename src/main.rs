use std::{env, io, path::Path};

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

fn get_git_package_sources(package: &lock_file::Package) -> manifest::Source {
    let name = package.name.clone();
    let source = package.source.clone().unwrap();

    let commit = Url::parse(&source)
        .unwrap()
        .fragment()
        .map(|f| f.to_string())
        .expect("The commit needs to be indicated in the fragement part");

    let canonical = canonical_url(&source).unwrap();
    let name = canonical.path_segments().unwrap().last().unwrap();
    let repo_url = canonical.to_string();

    let dest = format!("{name}-{}", &commit[..COMMIT_LEN]);
    eprintln!("{dest}");

    dbg!(&repo_url);

    manifest::Source::Git(manifest::Git {
        url: repo_url,
        commit,
        dest,
    })
}

fn get_package_sources(
    package: &lock_file::Package,
) -> Option<(Vec<manifest::Source>, toml::map::Map<String, toml::Value>)> {
    let name = &package.name;
    let version = &package.version;

    if let Some(source) = package.source.as_ref() {
        if source.starts_with("git+") {
            let source = get_git_package_sources(package);

            let c = toml::map::Map::new();
            return Some((vec![source], c));
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
        if let Some((mut pkg_sources, cargo_vendored_entry)) = get_package_sources(&package) {
            package_sources.append(&mut pkg_sources);

            for (key, value) in cargo_vendored_entry {
                cargo_vendored_sources.insert(key, value);
            }
        }
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
