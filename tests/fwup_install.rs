//! Drives `Fwup::install_async` against a real fwup and a real A/B image.
//!
//! Ignored by default: it needs `fwup` on PATH and it writes disk images, so it
//! runs in the container rather than on a laptop.
//!
//!     docker build --target test -t nerves-hub-link-agent:ci .
//!     docker run --rm -v "$PWD:/work" -w /work nerves-hub-link-agent:ci \
//!         cargo test --test fwup_install -- --ignored --nocapture
//!
//! What it checks is the seam the unit tests cannot reach: that the archive
//! streams into fwup's stdin without deadlocking, that `-n` progress comes back
//! as percentages, and that the slot fwup chose is the one that changed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use nerves_hub_link_agent::config::{FwupConfig, FwupMetadataSource};
use nerves_hub_link_agent::update_tool::fwup::Fwup;
use nerves_hub_link_agent::{FirmwareMeta, Stage, UpdatePayload};

const ROOTFS_A_LBA: u32 = 2048;
const ROOTFS_B_LBA: u32 = 4096;

#[tokio::test]
#[ignore = "needs fwup; run in the container"]
async fn an_update_streams_into_fwup_and_lands_in_the_free_slot() {
    let work = scratch("stream");

    let v1 = build_archive(&work, "1", "0.1.0");
    let v2 = build_archive(&work, "2", "0.2.0");

    let disk = work.join("disk.img");
    factory_write(&disk, &v1);

    assert_eq!(live_slot(&disk), ROOTFS_A_LBA, "factory write lands on A");
    assert_eq!(slot_contents(&disk, ROOTFS_A_LBA), "rootfs version 1");

    let (url, _server) = serve(&v2).await;
    let mut fwup = tool(&disk);

    let reports = Arc::new(Mutex::new(Vec::new()));
    let collected = Arc::clone(&reports);

    let installed = fwup
        .install_async(
            &update(&url, &std::fs::read(&v2).unwrap()),
            &reqwest::Client::new(),
            move |stage, percent| collected.lock().unwrap().push((stage, percent)),
        )
        .await
        .expect("install should succeed");

    // fwup chose the free slot by reading the disk, and rewrote the MBR to say
    // so. Neither of those was told to it by the agent.
    assert_eq!(live_slot(&disk), ROOTFS_B_LBA, "the upgrade flipped to B");
    assert_eq!(slot_contents(&disk, ROOTFS_B_LBA), "rootfs version 2");
    assert_eq!(
        slot_contents(&disk, ROOTFS_A_LBA),
        "rootfs version 1",
        "the slot that was running must be untouched"
    );

    assert_eq!(
        installed.bytes_transferred,
        std::fs::metadata(&v2).unwrap().len()
    );
    assert!(installed.reboot_required);

    let reports = reports.lock().unwrap();

    assert!(!reports.is_empty(), "fwup reported no progress at all");
    assert!(reports.iter().all(|(stage, _)| *stage == Stage::Updating));
    assert_eq!(
        reports.last().map(|(_, percent)| *percent),
        Some(100),
        "progress should reach 100"
    );

    // Monotonic, because the agent only forwards a percentage when it changes.
    let percentages: Vec<u8> = reports.iter().map(|(_, p)| *p).collect();
    let mut sorted = percentages.clone();
    sorted.sort_unstable();
    assert_eq!(
        percentages, sorted,
        "progress went backwards: {percentages:?}"
    );
}

#[tokio::test]
#[ignore = "needs fwup; run in the container"]
async fn a_checksum_that_does_not_match_is_reported() {
    let work = scratch("checksum");

    let v1 = build_archive(&work, "1", "0.1.0");
    let v2 = build_archive(&work, "2", "0.2.0");

    let disk = work.join("disk.img");
    factory_write(&disk, &v1);

    let (url, _server) = serve(&v2).await;
    let mut fwup = tool(&disk);

    let mut payload = update(&url, &std::fs::read(&v2).unwrap());
    payload.checksum = Some("0".repeat(64));

    let error = fwup
        .install_async(&payload, &reqwest::Client::new(), |_, _| {})
        .await
        .expect_err("a wrong checksum should not be reported as success");

    assert!(
        error.to_string().contains("checksum"),
        "unhelpful error: {error}"
    );
}

#[tokio::test]
#[ignore = "needs fwup; run in the container"]
async fn a_truncated_archive_fails_without_disturbing_the_running_slot() {
    let work = scratch("truncated");

    let v1 = build_archive(&work, "1", "0.1.0");
    let v2 = build_archive(&work, "2", "0.2.0");

    let disk = work.join("disk.img");
    factory_write(&disk, &v1);

    // Half an archive. fwup should refuse it rather than write half a slot.
    let whole = std::fs::read(&v2).unwrap();
    let half = &whole[..whole.len() / 2];
    let truncated = work.join("truncated.fw");
    std::fs::write(&truncated, half).unwrap();

    let (url, _server) = serve(&truncated).await;
    let mut fwup = tool(&disk);

    let error = fwup
        .install_async(&update(&url, half), &reqwest::Client::new(), |_, _| {})
        .await
        .expect_err("a truncated archive should fail");

    // The point of A/B: a failed write leaves the running system alone.
    assert_eq!(live_slot(&disk), ROOTFS_A_LBA);
    assert_eq!(slot_contents(&disk, ROOTFS_A_LBA), "rootfs version 1");

    println!("truncated archive reported: {error}");
}

// ---------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("fwup-install-{name}"));

    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();

    path
}

fn conf_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test/ab/fwup.conf")
}

/// Build a .fw whose rootfs payload says which version it is, so a test can
/// read a slot back and know what landed there.
fn build_archive(work: &Path, marker: &str, version: &str) -> PathBuf {
    let rootfs = work.join(format!("rootfs-{marker}.bin"));
    std::fs::write(&rootfs, format!("rootfs version {marker}\n")).unwrap();

    std::fs::OpenOptions::new()
        .write(true)
        .open(&rootfs)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();

    let out = work.join(format!("v{marker}.fw"));

    let status = Command::new("fwup")
        .args(["-c", "-f"])
        .arg(conf_path())
        .arg("-o")
        .arg(&out)
        .env("ROOTFS_PATH", &rootfs)
        .env("NH_PRODUCT", "test-product")
        .env("NH_VERSION", version)
        .env("NH_PLATFORM", "test")
        .env("NH_ARCHITECTURE", "x86_64")
        .status()
        .expect("fwup should be on PATH");

    assert!(status.success(), "fwup -c failed");

    out
}

fn factory_write(disk: &Path, archive: &Path) {
    let status = Command::new("fwup")
        .arg("-a")
        .arg("-d")
        .arg(disk)
        .arg("-i")
        .arg(archive)
        .args(["-t", "complete", "-U", "--quiet"])
        .status()
        .unwrap();

    assert!(status.success(), "factory write failed");
}

fn tool(disk: &Path) -> Fwup {
    Fwup::new(FwupConfig {
        device: disk.to_path_buf(),
        task: "upgrade".into(),
        binary: PathBuf::from("fwup"),
        public_key: None,
        extra_args: vec![],
        allow_block_device: false,
        confirm_command: None,
        metadata: FwupMetadataSource::default(),
    })
    .expect("fwup should be available")
}

fn update(url: &str, contents: &[u8]) -> UpdatePayload {
    use sha2::{Digest, Sha256};

    UpdatePayload {
        update_available: true,
        firmware_url: Some(url.to_string()),
        firmware_meta: Some(FirmwareMeta {
            uuid: "test-uuid".into(),
            version: Some("0.2.0".into()),
            product: Some("test-product".into()),
            platform: Some("test".into()),
            architecture: Some("x86_64".into()),
        }),
        size: Some(contents.len() as u64),
        checksum: Some(format!("{:X}", Sha256::digest(contents))),
        deployment_id: None,
    }
}

/// Which slot the MBR names as partition 0 — the live one.
fn live_slot(disk: &Path) -> u32 {
    let raw = std::fs::read(disk).unwrap();

    // Partition entry 0 is at byte 446; its start-LBA is the u32 at offset 8.
    u32::from_le_bytes(raw[454..458].try_into().unwrap())
}

fn slot_contents(disk: &Path, lba: u32) -> String {
    let raw = std::fs::read(disk).unwrap();
    let start = lba as usize * 512;

    String::from_utf8_lossy(&raw[start..start + 64])
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches('\0')
        .to_string()
}

/// The smallest HTTP server that can hand over one file.
///
/// A real one would be a dependency and a fixture directory; this is twenty
/// lines and serves exactly the one thing the test needs.
async fn serve(file: &Path) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = std::fs::read(file).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/firmware.fw", listener.local_addr().unwrap());

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            let body = body.clone();

            tokio::spawn(async move {
                let mut scratch = [0u8; 2048];
                let _ = socket.read(&mut scratch).await;

                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );

                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    (url, handle)
}
