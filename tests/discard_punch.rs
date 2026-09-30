//! Discarding qcow2 data clusters must return their space to the host file.

use imago::file::File;
use imago::qcow2::Qcow2;
use imago::{
    FormatAccess, FormatCreateBuilder, FormatDriverBuilder, PermissiveImplicitOpenGate, Storage,
    StorageCreateOptions, StorageOpenOptions,
};
use std::future::Future;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const MB: u64 = 1 << 20;
const CLUSTER: u64 = 64 << 10;

fn run<F: Future<Output = io::Result<()>>>(f: F) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
        .unwrap();
}

/// Bytes actually allocated to the file on the host.
fn allocated(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().blocks() * 512
}

async fn create(path: &Path, size: u64, backing: Option<&Path>) -> io::Result<()> {
    let opts = StorageCreateOptions::new().filename(path).size(0);
    let file = File::create_open(opts).await?;
    let mut builder = Qcow2::<File>::create_builder(file)
        .size(size)
        .cluster_size(CLUSTER as usize);
    if let Some(backing) = backing {
        builder = builder.backing(backing.to_str().unwrap().to_string(), "raw".to_string());
    }
    builder.create().await
}

async fn open(path: &Path) -> io::Result<FormatAccess<File>> {
    let qcow2 = Qcow2::<File>::builder_path(path)
        .write(true)
        .open(PermissiveImplicitOpenGate::default())
        .await?;
    Ok(FormatAccess::new(qcow2))
}

async fn read(img: &FormatAccess<File>, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    img.read(&mut buf[..], offset).await?;
    Ok(buf)
}

fn assert_all(buf: &[u8], byte: u8, what: &str) {
    if let Some(pos) = buf.iter().position(|&b| b != byte) {
        panic!(
            "{what}: byte {pos} is 0x{:02x}, expected 0x{byte:02x}",
            buf[pos]
        );
    }
}

#[test]
fn discard_punches_freed_clusters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    run(async {
        create(&path, 64 * MB, None).await?;
        let mut img = open(&path).await?;

        img.write(&vec![0xaa; (8 * MB) as usize][..], 0).await?;
        img.flush().await?;
        let before = allocated(&path);
        assert!(before >= 8 * MB, "data not allocated: {before}");

        img.discard_to_any(0, 8 * MB).await?;
        img.flush().await?;
        let after = allocated(&path);
        assert!(
            after + 7 * MB <= before,
            "discard did not free host space: before {before}, after {after}"
        );
        assert_all(&read(&img, 0, 8 * MB).await?, 0, "discarded range");
        Ok(())
    });
}

#[test]
fn write_after_discard_reuses_clusters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    run(async {
        create(&path, 64 * MB, None).await?;
        let mut img = open(&path).await?;

        img.write(&vec![0xaa; (8 * MB) as usize][..], 0).await?;
        img.discard_to_any(0, 8 * MB).await?;
        img.flush().await?;
        let len_after_discard = std::fs::metadata(&path)?.len();
        let punched = allocated(&path);

        img.write(&vec![0x55; (4 * MB) as usize][..], 2 * MB)
            .await?;
        img.flush().await?;
        assert!(allocated(&path) >= punched + 4 * MB);
        // Freed clusters are reused instead of growing the file
        assert!(std::fs::metadata(&path)?.len() <= len_after_discard.max(9 * MB));

        drop(img);
        let img = open(&path).await?;
        assert_all(&read(&img, 0, 2 * MB).await?, 0, "before rewrite");
        assert_all(&read(&img, 2 * MB, 4 * MB).await?, 0x55, "rewritten");
        assert_all(&read(&img, 6 * MB, 2 * MB).await?, 0, "after rewrite");
        Ok(())
    });
}

#[test]
fn discard_does_not_expose_backing_data() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.raw");
    let path = dir.path().join("disk.qcow2");
    std::fs::write(&base, vec![0x11; (4 * MB) as usize]).unwrap();
    run(async {
        create(&path, 4 * MB, Some(&base)).await?;
        let mut img = open(&path).await?;

        img.write(&vec![0xaa; MB as usize][..], 0).await?;
        img.flush().await?;
        let before = allocated(&path);

        img.discard_to_any(0, MB).await?;
        img.flush().await?;
        assert!(
            allocated(&path) + MB / 2 <= before,
            "discard did not free host space"
        );

        drop(img);
        let img = open(&path).await?;
        assert_all(&read(&img, 0, MB).await?, 0, "discarded range");
        assert_all(&read(&img, MB, MB).await?, 0x11, "untouched range");
        Ok(())
    });
}

#[test]
fn discard_of_last_cluster_then_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    run(async {
        create(&path, 64 * MB, None).await?;
        let mut img = open(&path).await?;

        // The last guest cluster written is the last host cluster in the file
        img.write(&vec![0xaa; MB as usize][..], 0).await?;
        img.write(&vec![0xbb; CLUSTER as usize][..], 32 * MB)
            .await?;
        img.flush().await?;
        let len = std::fs::metadata(&path)?.len();

        img.discard_to_any(32 * MB, CLUSTER).await?;
        img.flush().await?;
        assert!(std::fs::metadata(&path)?.len() <= len);

        img.write(&vec![0xcc; (2 * CLUSTER) as usize][..], 40 * MB)
            .await?;
        img.flush().await?;

        drop(img);
        let img = open(&path).await?;
        assert_all(&read(&img, 0, MB).await?, 0xaa, "first range");
        assert_all(&read(&img, 32 * MB, CLUSTER).await?, 0, "discarded cluster");
        assert_all(&read(&img, 40 * MB, 2 * CLUSTER).await?, 0xcc, "new write");
        Ok(())
    });
}

#[test]
fn partial_cluster_discard_keeps_allocation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    run(async {
        create(&path, 64 * MB, None).await?;
        let mut img = open(&path).await?;

        img.write(&vec![0xaa; CLUSTER as usize][..], 0).await?;
        img.flush().await?;
        let before = allocated(&path);

        // Smaller than a cluster: nothing can be freed
        img.discard_to_any(4096, 8192).await?;
        img.flush().await?;
        assert_eq!(allocated(&path), before);
        assert_all(&read(&img, 0, CLUSTER).await?, 0xaa, "discard_to_any");

        // discard_to_zero must still make the range read as zeroes
        img.discard_to_zero(4096, 8192).await?;
        img.flush().await?;
        assert_all(&read(&img, 0, 4096).await?, 0xaa, "head");
        assert_all(&read(&img, 4096, 8192).await?, 0, "zeroed range");
        assert_all(&read(&img, 12288, CLUSTER - 12288).await?, 0xaa, "tail");
        Ok(())
    });
}

#[test]
fn raw_storage_open_still_works() {
    // Opening through generic storage options (as libkrun does) keeps working
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disk.qcow2");
    run(async {
        create(&path, 16 * MB, None).await?;
        let file = File::open(StorageOpenOptions::new().write(true).filename(&path)).await?;
        let qcow2 = Qcow2::<File>::builder(file)
            .write(true)
            .open(PermissiveImplicitOpenGate::default())
            .await?;
        assert_eq!(qcow2.cluster_size() as u64, CLUSTER);
        Ok(())
    });
}
