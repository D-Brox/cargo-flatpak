
mod sources;
mod cli;


use std::{collections::HashMap, fs::File, io::Write};

use cargo_metadata::MetadataCommand;
use clap::Parser;
use cli::Command;
use sources::{get_package_sources, Inline, LockFile, Source};

const CRATES_IO: &str = "https://static.crates.io/crates";
const CARGO_HOME: &str = "cargo";
const CARGO_CRATES: &str = "cargo/vendor";
const VENDORED_SOURCES: &str = "vendored-sources";
const GIT_CACHE: &str = "flatpak-cargo/git";
const COMMIT_LEN: usize = 7;

fn main() {
    let Command::Flatpak(args) = Command::parse();
    let cargo_metadata = MetadataCommand::new().exec().expect("failed to get metadata");
    let workspace = cargo_metadata.workspace_root.as_std_path();
    let lockfile = workspace.join("Cargo.lock");

    let cargo_lock = std::fs::read_to_string(&lockfile).unwrap();
    let cargo_lock: LockFile = toml::de::from_str(&cargo_lock).unwrap();
    let mut manifests = HashMap::new();
    for package in cargo_metadata.packages {
        manifests.insert(package.name, package.manifest_path.to_string());
    }

    let mut package_sources: Vec<Source> = Vec::new();

    let mut cargo_vendored_sources = toml::map::Map::new();
    cargo_vendored_sources.insert(VENDORED_SOURCES.into(), {
        let mut obj = toml::map::Map::new();
        obj.insert("directory".into(), CARGO_CRATES.into());
        obj.into()
    });

    for package in cargo_lock.package {
        if let Some((mut pkg_sources, cargo_vendored_entry)) =
            get_package_sources(&package,manifests.get(&package.name).expect("package not in the metadata"))
        {
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

        Source::Inline(Inline {
            contents: source,
            dest: CARGO_HOME.into(),
            dest_filename: "config".into(),
        })
    };

    sources.push(cargo_vendored_sources);

    let mut file = File::create(workspace.join(args.output)).expect("Could not create file!");
    file.write_all(serde_json::to_string_pretty(&sources).unwrap().as_bytes())
        .expect("Cannot write to the file!");
}