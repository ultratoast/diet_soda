//! Acceptance coverage for local skill installation and archive safety.
//! These tests intentionally use only temporary files and in-memory archives.
//! A trusted HTTPS success case is not included: production constructs its
//! reqwest client with the platform trust roots and exposes no test client or
//! certificate-root injection point. The insecure HTTP rejection is tested
//! locally instead of weakening TLS verification.

use diet_soda::skills;
use flate2::{write::GzEncoder, Compression};
use std::{fs, path::Path};
use tar::{Builder, Header};

fn valid_skill(name: &str) -> String {
    format!("---\nname: {name}\ndescription: Acceptance fixture.\n---\nInstructions for {name}.\n")
}

fn write_skill(directory: &Path, name: &str) {
    fs::create_dir_all(directory).unwrap();
    fs::write(directory.join("SKILL.md"), valid_skill(name)).unwrap();
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let output = Vec::new();
    let encoder = GzEncoder::new(output, Compression::default());
    let mut builder = Builder::new(encoder);
    for (path, contents) in entries {
        let mut header = Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, *path, *contents).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_entry_count(count: usize) -> Vec<u8> {
    let output = Vec::new();
    let encoder = GzEncoder::new(output, Compression::default());
    let mut builder = Builder::new(encoder);
    for index in 0..count {
        let path = format!("files/{index}.txt");
        let mut header = Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, &b"x"[..]).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_symlink() -> Vec<u8> {
    let output = Vec::new();
    let encoder = GzEncoder::new(output, Compression::default());
    let mut builder = Builder::new(encoder);
    let mut header = Header::new_gnu();
    header.set_path("link").unwrap();
    header.set_entry_type(tar::EntryType::symlink());
    header.set_size(0);
    header.set_link_name("outside").unwrap();
    header.set_mode(0o777);
    header.set_cksum();
    builder.append(&header, &[][..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

fn archive_with_traversal() -> Vec<u8> {
    let output = Vec::new();
    let encoder = GzEncoder::new(output, Compression::default());
    let mut builder = Builder::new(encoder);
    let mut header = Header::new_gnu();
    header.set_path("placeholder").unwrap();
    header.set_size(0);
    header.set_mode(0o644);
    let bytes = header.as_mut_bytes();
    bytes[..100].fill(0);
    bytes[..11].copy_from_slice(b"../SKILL.md");
    header.set_cksum();
    builder.append(&header, &[][..]).unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

async fn assert_failure_cleans_staging<F>(destination: &Path, operation: F)
where
    F: std::future::Future<Output = anyhow::Result<std::path::PathBuf>>,
{
    assert!(operation.await.is_err());
    let leftovers: Vec<_> = fs::read_dir(destination)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".install-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging directories remained: {leftovers:?}"
    );
}

#[tokio::test]
async fn installs_standalone_local_skill_file() {
    let destination = tempfile::tempdir().unwrap();
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills/standalone/SKILL.md");

    let installed = skills::install(source.to_str().unwrap(), destination.path())
        .await
        .unwrap();

    assert_eq!(installed.file_name().unwrap(), "fixture-standalone");
    assert_eq!(
        fs::read_to_string(installed.join("SKILL.md")).unwrap(),
        fs::read_to_string(source).unwrap()
    );
}

#[tokio::test]
async fn installs_local_skill_directory() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    write_skill(&source, "local-directory");
    fs::write(source.join("REFERENCE.md"), "supporting material").unwrap();
    let destination = root.path().join("installed");

    let installed = skills::install(source.to_str().unwrap(), &destination)
        .await
        .unwrap();

    assert!(installed.join("REFERENCE.md").is_file());
    assert_eq!(skills::read(&installed).unwrap().name, "local-directory");
}

#[tokio::test]
async fn installs_tar_gz_and_flattens_single_top_level_directory() {
    let root = tempfile::tempdir().unwrap();
    let archive_path = root.path().join("skill.tar.gz");
    fs::write(
        &archive_path,
        archive(&[("bundle/SKILL.md", valid_skill("flattened").as_bytes())]),
    )
    .unwrap();

    let installed = skills::install(
        archive_path.to_str().unwrap(),
        &root.path().join("installed"),
    )
    .await
    .unwrap();

    assert_eq!(installed.file_name().unwrap(), "flattened");
    assert_eq!(skills::read(&installed).unwrap().name, "flattened");
}

#[tokio::test]
async fn rejects_duplicate_skill_without_leaving_staging() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("skill.md");
    fs::write(&source, valid_skill("duplicate")).unwrap();
    let destination = root.path().join("installed");
    skills::install(source.to_str().unwrap(), &destination)
        .await
        .unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
}

#[tokio::test]
async fn rejects_archive_path_traversal() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("traversal.tar.gz");
    fs::write(&source, archive_with_traversal()).unwrap();
    let destination = root.path().join("installed");
    fs::create_dir_all(&destination).unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
    assert!(!root.path().join("SKILL.md").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_archive_entry() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("symlink.tar.gz");
    fs::write(&source, archive_with_symlink()).unwrap();
    let destination = root.path().join("installed");
    fs::create_dir_all(&destination).unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_special_archive_entry() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("special.tar.gz");
    let output = Vec::new();
    let encoder = GzEncoder::new(output, Compression::default());
    let mut builder = Builder::new(encoder);
    let mut header = Header::new_gnu();
    header.set_path("device").unwrap();
    header.set_entry_type(tar::EntryType::character_special());
    header.set_size(0);
    header.set_mode(0o644);
    header.set_device_major(1).unwrap();
    header.set_device_minor(3).unwrap();
    header.set_cksum();
    builder.append(&header, &[][..]).unwrap();
    let bytes = builder.into_inner().unwrap().finish().unwrap();
    fs::write(&source, bytes).unwrap();
    let destination = root.path().join("installed");
    fs::create_dir_all(&destination).unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
}

#[tokio::test]
async fn rejects_archive_entry_count_limit() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("too-many.tar.gz");
    fs::write(&source, archive_with_entry_count(2001)).unwrap();
    let destination = root.path().join("installed");
    fs::create_dir_all(&destination).unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
}

#[tokio::test]
async fn rejects_archive_unpacked_size_limit() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("too-large.tar.gz");
    let contents = vec![b'x'; 20_000_001];
    fs::write(&source, archive(&[("SKILL.md", &contents)])).unwrap();
    let destination = root.path().join("installed");
    fs::create_dir_all(&destination).unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
}

#[tokio::test]
async fn rejects_local_download_size_limit() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("too-large.bin");
    let file = fs::File::create(&source).unwrap();
    file.set_len(10_000_001).unwrap();
    let destination = root.path().join("installed");
    fs::create_dir_all(&destination).unwrap();

    assert_failure_cleans_staging(
        &destination,
        skills::install(source.to_str().unwrap(), &destination),
    )
    .await;
}

#[tokio::test]
async fn rejects_insecure_http_skill_url_without_network_access() {
    let destination = tempfile::tempdir().unwrap();

    let result = skills::install("http://127.0.0.1:1/skill.tar.gz", destination.path()).await;

    assert!(result.is_err());
    let leftovers: Vec<_> = fs::read_dir(destination.path()).unwrap().collect();
    assert!(leftovers.is_empty(), "insecure URL created local artifacts");
}
