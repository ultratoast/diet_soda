//! Skill discovery and staged installation. Skill files are data; scripts are
//! never executed as a side effect of installing or loading their instructions.
use crate::{
    config::{valid_name, Config},
    fsutil, tools,
};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub directory: PathBuf,
}
#[derive(Deserialize)]
struct Metadata {
    name: String,
    description: String,
}

pub fn read(directory: &Path) -> Result<Skill> {
    let path = directory.join("SKILL.md");
    if std::fs::metadata(&path)?.len() > 256_000 {
        bail!("SKILL.md exceeds 256 KB");
    }
    let text = std::fs::read_to_string(path)?;
    parse(&text, directory)
}
fn parse(text: &str, directory: &Path) -> Result<Skill> {
    let normalized = text.replace("\r\n", "\n");
    let rest = normalized
        .strip_prefix("---\n")
        .context("SKILL.md requires YAML frontmatter with name and description")?;
    let (yaml, instructions) = rest
        .split_once("\n---\n")
        .context("Unclosed skill frontmatter")?;
    let meta: Metadata = serde_yaml::from_str(yaml)?;
    if !valid_name(&meta.name) || meta.description.is_empty() || instructions.trim().is_empty() {
        bail!("Skill name, description and instructions are required");
    }
    Ok(Skill {
        name: meta.name,
        description: meta.description,
        instructions: instructions.trim().into(),
        directory: directory.into(),
    })
}
pub fn discover(config: &Config) -> Result<Vec<Skill>> {
    let mut result: Vec<Skill> = vec![];
    for dir in std::iter::once(&config.skills_dir).chain(&config.skills.directories) {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() && entry.path().join("SKILL.md").exists() {
                let skill = read(&entry.path())
                    .with_context(|| format!("Skill {}", entry.path().display()))?;
                if result.iter().any(|s| s.name == skill.name) {
                    bail!("Duplicate skill {}", skill.name);
                }
                result.push(skill);
            }
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}
pub fn instructions(config: &Config, enabled: &[String]) -> Result<String> {
    let skills = discover(config)?;
    let mut out = String::new();
    if !skills.is_empty() {
        out.push_str("\nAvailable skills (use load_skill to read one):\n");
        for skill in &skills {
            out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
    }
    for name in enabled {
        let skill = skills
            .iter()
            .find(|s| &s.name == name)
            .with_context(|| format!("Enabled skill not found: {name}"))?;
        out.push_str(&format!(
            "\n<skill name=\"{}\" directory=\"{}\">\n{}\n</skill>\n",
            skill.name,
            skill.directory.display(),
            skill.instructions
        ));
    }
    Ok(out)
}

pub async fn install(source: &str, destination: &Path) -> Result<PathBuf> {
    fsutil::create_dir_all_private(destination)?;
    let stage = destination.join(format!(".install-{}", uuid::Uuid::new_v4()));
    fsutil::create_dir_all_private(&stage)?;
    let result = async {
        let path = Path::new(source);
        if path.is_dir() {
            let mut total = 0;
            let mut count = 0;
            copy_directory(path, &stage, &mut total, &mut count)?;
        } else {
            let (bytes, truncated) =
                if source.starts_with("https://") || source.starts_with("http://") {
                    // `install` has no cancellation parameter; give the hardened
                    // download a fresh token so its DNS/connect/read races still
                    // short-circuit on a cancelled token if one is ever threaded
                    // through here.
                    tools::download_https(source, 10_000_000, &CancellationToken::new()).await?
                } else {
                    if std::fs::metadata(path)?.len() > 10_000_000 {
                        bail!("Skill archive exceeds 10 MB");
                    }
                    (std::fs::read(path)?, false)
                };
            if truncated {
                bail!("Skill download exceeds 10 MB");
            }
            if bytes.starts_with(&[0x1f, 0x8b]) {
                unpack(&bytes, &stage)?;
                fsutil::harden_tree_private(&stage)?;
            } else {
                parse(std::str::from_utf8(&bytes)?, &stage)?;
                let mut file = fsutil::private_open_options()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(stage.join("SKILL.md"))?;
                file.write_all(&bytes)?;
            }
        }
        let root = if stage.join("SKILL.md").exists() {
            stage.clone()
        } else {
            let dirs: Vec<_> = std::fs::read_dir(&stage)?.collect::<std::io::Result<Vec<_>>>()?;
            if dirs.len() != 1 || !dirs[0].path().join("SKILL.md").exists() {
                bail!("Archive needs SKILL.md at its root or in a single top-level directory");
            }
            dirs[0].path()
        };
        let skill = read(&root)?;
        let target = destination.join(&skill.name);
        if target.exists() {
            bail!("Skill {} is already installed", skill.name);
        }
        std::fs::rename(root, &target)?;
        Ok(target)
    }
    .await;
    if stage.exists() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}
fn copy_directory(source: &Path, dest: &Path, total: &mut u64, count: &mut usize) -> Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = dest.join(entry.file_name());
        *count += 1;
        if *count > 2000 {
            bail!("Skill exceeds 2000 files");
        }
        if kind.is_symlink() {
            bail!("Skill installation does not accept symlinks");
        }
        if kind.is_dir() {
            fsutil::create_dir_all_private(&target)?;
            copy_directory(&entry.path(), &target, total, count)?;
        } else if kind.is_file() {
            *total += entry.metadata()?.len();
            if *total > 20_000_000 {
                bail!("Skill exceeds 20 MB unpacked");
            }
            let mut source = std::fs::File::open(entry.path())?;
            let mut file = fsutil::private_open_options()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&target)?;
            std::io::copy(&mut source, &mut file)?;
        } else {
            bail!("Unsupported skill file type");
        }
    }
    Ok(())
}
fn unpack(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut total = 0;
    for (index, entry) in archive.entries()?.enumerate() {
        if index >= 2000 {
            bail!("Skill archive exceeds 2000 entries");
        }
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        if !path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        {
            bail!("Unsafe skill archive path");
        }
        if !kind.is_file() && !kind.is_dir() {
            bail!("Skill archive may contain only files and directories");
        }
        total += entry.size();
        if total > 20_000_000 {
            bail!("Skill exceeds 20 MB unpacked");
        }
        if !entry.unpack_in(dest)? {
            bail!("Unsafe skill archive path");
        }
    }
    Ok(())
}
