//! Resource-read regressions requested alongside the IWD completeness fix.

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use asset_transport::{IwdIndex, IwdSoundIndex, read_iwd_named};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

struct FixtureDir(PathBuf);

impl FixtureDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "iw4l-resource-reads-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// All payloads are synthetic. Keep the compressed data and its CRC valid,
// changing only the advertised uncompressed size (or explicitly the CRC).
fn write_entry(
    path: &Path,
    name: &str,
    payload: &[u8],
    method: CompressionMethod,
    declared: u64,
    bad_crc: bool,
) {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            name,
            SimpleFileOptions::default().compression_method(method),
        )
        .unwrap();
    writer.write_all(payload).unwrap();
    let mut bytes = writer.finish().unwrap().into_inner();
    let central = bytes.windows(4).position(|b| b == b"PK\x01\x02").unwrap();
    if let Ok(size) = u32::try_from(declared) {
        bytes[22..26].copy_from_slice(&size.to_le_bytes());
        bytes[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
    } else {
        // A ZIP64 uncompressed-size field, without allocating a large payload.
        bytes[central + 24..central + 28].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[central + 30..central + 32].copy_from_slice(&12u16.to_le_bytes());
        let extra_at = central + 46 + name.len();
        let mut extra = vec![1, 0, 8, 0];
        extra.extend_from_slice(&declared.to_le_bytes());
        drop(bytes.splice(extra_at..extra_at, extra));
        let end = bytes.windows(4).position(|b| b == b"PK\x05\x06").unwrap();
        let central_size = u32::from_le_bytes(bytes[end + 12..end + 16].try_into().unwrap());
        bytes[end + 12..end + 16].copy_from_slice(&(central_size + 12).to_le_bytes());
    }
    if bad_crc {
        bytes[14] ^= 1;
        bytes[central + 16] ^= 1;
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn iwd_payload_length_is_checked_for_images_sounds_and_named_reads() {
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        for declared in [2, 4, 8] {
            let dir = FixtureDir::new();
            write_entry(
                &dir.0.join("image.iwd"),
                "images/probe.iwi",
                b"1234",
                method,
                declared,
                false,
            );
            write_entry(
                &dir.0.join("sound.iwd"),
                "sound/probe.wav",
                b"1234",
                method,
                declared,
                false,
            );
            let index = IwdIndex::open(&dir.0).unwrap();
            let image = &index.image_candidates("PROBE").unwrap()[0];
            let sound = IwdSoundIndex::open(&dir.0).unwrap();
            if declared == 4 {
                assert_eq!(image.read().unwrap(), b"1234");
                assert_eq!(sound.read_sound("PROBE.WAV").unwrap().unwrap(), b"1234");
                assert_eq!(
                    read_iwd_named(&dir.0, "IMAGES\\probe.iwi").unwrap(),
                    b"1234"
                );
            } else {
                assert!(image.read().unwrap_err().contains("length mismatch"));
                assert!(
                    sound
                        .read_sound("probe.wav")
                        .unwrap()
                        .unwrap_err()
                        .contains("length mismatch")
                );
                assert!(read_iwd_named(&dir.0, "images/probe.iwi").is_none());
            }
        }
    }
}

#[test]
fn iwd_header_prefix_requires_its_bytes_and_checks_crc_at_entry_end() {
    let dir = FixtureDir::new();
    write_entry(
        &dir.0.join("valid.iwd"),
        "images/valid.iwi",
        b"1234",
        CompressionMethod::Stored,
        4,
        false,
    );
    write_entry(
        &dir.0.join("short.iwd"),
        "images/short.iwi",
        b"1234",
        CompressionMethod::Stored,
        8,
        false,
    );
    write_entry(
        &dir.0.join("crc.iwd"),
        "images/crc.iwi",
        b"1234",
        CompressionMethod::Stored,
        4,
        true,
    );
    let index = IwdIndex::open(&dir.0).unwrap();
    let valid = &index.image_candidates("valid").unwrap()[0];
    for limit in [0, 2, 4, 5] {
        assert_eq!(valid.read_header(limit).unwrap(), &b"1234"[..limit.min(4)]);
    }
    let short = &index.image_candidates("short").unwrap()[0];
    assert_eq!(short.read_header(2).unwrap(), b"12");
    for limit in [6, 8, 32] {
        assert!(
            short
                .read_header(limit)
                .unwrap_err()
                .contains("length mismatch")
        );
    }
    let crc = &index.image_candidates("crc").unwrap()[0];
    assert_eq!(crc.read_header(2).unwrap(), b"12");
    assert!(crc.read_header(4).is_err());
    assert!(crc.read_header(5).is_err());
    assert!(crc.read().is_err());
}

#[test]
fn iwd_zip64_size_is_rejected_for_payload_but_does_not_truncate_prefix() {
    let dir = FixtureDir::new();
    write_entry(
        &dir.0.join("huge.iwd"),
        "images/huge.iwi",
        b"1234",
        CompressionMethod::Stored,
        u64::MAX,
        false,
    );
    let index = IwdIndex::open(&dir.0).unwrap();
    let image = &index.image_candidates("huge").unwrap()[0];
    assert!(image.read().is_err());
    assert_eq!(image.read_header(2).unwrap(), b"12");
    assert!(read_iwd_named(&dir.0, "images/huge.iwi").is_none());
}

#[test]
fn iwd_named_read_skips_incomplete_candidate_in_existing_archive_order() {
    let dir = FixtureDir::new();
    write_entry(
        &dir.0.join("a.iwd"),
        "default_xboxlive.cfg",
        b"set x 1\n",
        CompressionMethod::Stored,
        12,
        false,
    );
    write_entry(
        &dir.0.join("b.iwd"),
        "default_xboxlive.cfg",
        b"set x 2\n",
        CompressionMethod::Stored,
        8,
        false,
    );
    assert_eq!(
        read_iwd_named(&dir.0, "default_xboxlive.cfg").unwrap(),
        b"set x 2\n"
    );
}
