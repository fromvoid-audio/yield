//! Controlled real packer/cryptography/ZIP fixtures; never production-key/native qualification.
use super::*;
use crate::payload_selection::{
    Installed, Lease, PayloadSelectionService, SelectionCancellation, catalog, protocol,
};
use crate::update_catalog::{Channel, Platform};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

pub(super) struct Fixture {
    service: PayloadSelectionService,
    pub(super) selected: SelectedPayload,
    pub(super) archive: Vec<u8>,
    pub(super) checksum: Vec<u8>,
    pub(super) signature: Vec<u8>,
    pub(super) key: SigningKey,
}
impl Fixture {
    pub(super) fn new(bytes: Vec<u8>, version: &str, lane: Channel, platform: Platform) -> Self {
        let key = SigningKey::from_bytes(&[0x25; 32]);
        let version_parsed = semver::Version::parse(version).unwrap();
        let names = catalog::inventory(&version_parsed, lane).unwrap();
        let target_name =
            crate::update_catalog::asset_name(&version_parsed, lane, platform).unwrap();
        let target_hash: [u8; 32] = Sha256::digest(&bytes).into();
        let other_hash: [u8; 32] = Sha256::digest(b"unfetched controlled target metadata").into();
        let mut checksum = String::new();
        for name in names.iter().filter(|n| n.ends_with(".zip")) {
            checksum.push_str(&format!(
                "{}  {name}\n",
                hex(if *name == target_name {
                    target_hash
                } else {
                    other_hash
                })
            ));
        }
        let checksum = checksum.into_bytes();
        let signature = STANDARD.encode(key.sign(&checksum).to_bytes()).into_bytes();
        let files=names.into_iter().map(|name|{
            let (hash,size)=if name==target_name {(target_hash,bytes.len())}
            else if name.ends_with(".txt") {(Sha256::digest(&checksum).into(),checksum.len())}
            else if name.ends_with(".sig") {(Sha256::digest(&signature).into(),signature.len())}
            else {(other_hash,64)};
            json!({"name":name,"sha256":hex(hash),"size_bytes":size,"url":format!("/yield/assets/{name}")})
        }).collect::<Vec<_>>();
        let installed = Installed {
            version: semver::Version::parse("0.19.1").unwrap(),
            full_version: "0.19.1".into(),
            core: [0, 19, 1],
            number: 7,
            commit: [0xab; 20],
            platform,
        };
        let service =
            PayloadSelectionService::new(installed.clone(), protocol::Transport::Production);
        service.issuer.generation.store(1, Ordering::Release);
        let raw=serde_json::to_vec(&json!({"releases":[{"build_id":format!("yield-{version}"),"version":version,"build_number":8,"commit":COMMIT,"channel":crate::update_catalog::channel_name(lane),"files":files}]})).unwrap();
        let release = catalog::select(&raw, &installed, lane).unwrap();
        let gate = serde_json::to_vec(
            &json!({"build_id":release.build_id,"download_token":"controlled test token"}),
        )
        .unwrap();
        let requests = protocol::gate_requests(&gate, &release, lane, platform).unwrap();
        let selected = SelectedPayload {
            release,
            installed,
            channel: lane,
            requests,
            lease: Arc::new(Lease {
                issuer: Arc::downgrade(&service.issuer),
                generation: 1,
                cancellation: SelectionCancellation::default(),
                closed: AtomicBool::new(false),
            }),
        };
        Self {
            service,
            selected,
            archive: bytes,
            checksum,
            signature,
            key,
        }
    }
    fn verify(self) -> Result<(PayloadSelectionService, VerifiedPayload), SelectionError> {
        let result = verify_inner(
            self.selected,
            self.archive,
            self.checksum,
            self.signature,
            Key::Fixture(self.key.verifying_key()),
            Instant::now() + Duration::from_secs(300),
        )?;
        Ok((self.service, result))
    }
    fn refresh_signature_declaration(&mut self) {
        let f = self
            .selected
            .release
            .files
            .iter_mut()
            .find(|f| f.name.ends_with(".sig"))
            .unwrap();
        f.sha256 = Sha256::digest(&self.signature).into();
        f.size_bytes = self.signature.len() as u64;
    }
}
fn hex(hash: [u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}
fn reject<T>(result: Result<T, SelectionError>) -> SelectionError {
    match result {
        Ok(_) => panic!("unsafe archive was accepted"),
        Err(e) => e,
    }
}
pub(super) fn packed(version: &str, lane: Channel, platform: Platform) -> Vec<u8> {
    let temp = tempfile::tempdir().unwrap();
    let (_, _, target) = platform.labels();
    let binary = temp.path().join(format!(
        "target/{target}/release/{}",
        if platform == Platform::WindowsX64 {
            "yield.exe"
        } else {
            "yield"
        }
    ));
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(
        binary,
        b"controlled packer fixture, not a native executable",
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("assets")).unwrap();
    std::fs::write(
        temp.path().join("assets/logo3.icns"),
        b"controlled icon fixture",
    )
    .unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/internal/release/build_release_zip.sh");
    let (label, arch, _) = platform.labels();
    let output = Command::new("bash")
        .arg(script)
        .current_dir(temp.path())
        .env("YIELD_SKIP_BUILD", "1")
        .env("YIELD_MACOS_SIGNING", "0")
        .args([
            "--target",
            target,
            "--platform",
            label,
            "--arch",
            arch,
            "--channel",
            crate::update_catalog::channel_name(lane),
            "--version",
            version,
            "--target-version",
            "0.19.2",
            "--build-number",
            "8",
            "--git-sha",
            COMMIT,
            "--build-date",
            "2026-10-03",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "real packer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let name = crate::update_catalog::asset_name(
        &semver::Version::parse(version).unwrap(),
        lane,
        platform,
    )
    .unwrap();
    std::fs::read(temp.path().join("dist/release").join(name)).unwrap()
}
const ZIP_SCRIPT: &str = r#"
import io,json,sys,zipfile,stat,struct
config=json.load(sys.stdin)
manifest={'app':'yield','channel':'stable','target':'x86_64-pc-windows-msvc','platform':'windows','arch':'x86_64','version':'0.19.2','target_version':'0.19.2','commit':'0123456789abcdef0123456789abcdef01234567','build_date':'2026-10-03'}
members=[('yield/yield.exe',b'controlled binary',stat.S_IFREG|0o755)]
if config.get('binary_hex'):members[0]=('yield/yield.exe',bytes.fromhex(config['binary_hex']),stat.S_IFREG|0o755)
for name,text,kind in config.get('extra',[]): members.append((name,text.encode(),kind))
if config.get('remove_binary'):members=members[1:]
manifest['files']=[name.removeprefix('yield/') for name,_,kind in members if stat.S_IFMT(kind)!=stat.S_IFDIR]+['update-manifest.json']
manifest.update(config.get('manifest',{}))
raw=json.dumps(manifest,indent=2).encode()+b'\n'
if config.get('duplicate_manifest_field'):raw=raw[:-2]+b', "app":"yield"}\n'
members.append(('yield/update-manifest.json',raw,stat.S_IFREG|0o644))
class Pipe(io.BytesIO):
 def seekable(self):return False
 def seek(self,*args):raise io.UnsupportedOperation('nonseekable')
out=Pipe() if config.get('descriptor') else io.BytesIO()
with zipfile.ZipFile(out,'w',compression=zipfile.ZIP_DEFLATED) as z:
 for name,data,kind in members:
  info=zipfile.ZipInfo(name);info.create_system=3;info.external_attr=kind<<16
  padding=config.get('extra_padding',0)
  if padding:info.extra=struct.pack('<HH',0xf00d,padding)+b'x'*padding
  if stat.S_IFMT(kind)==stat.S_IFDIR:info.external_attr|=0x10
  info.compress_type=zipfile.ZIP_DEFLATED if config.get('deflate',True) else zipfile.ZIP_STORED
  with z.open(info,'w',force_zip64=config.get('zip64',False)) as f:f.write(data)
sys.stdout.buffer.write(out.getvalue())
"#;
pub(super) fn python_zip(config: Value) -> Vec<u8> {
    use std::io::Write;
    let mut child = Command::new("python3")
        .args(["-c", ZIP_SCRIPT])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        input.write_all(config.to_string().as_bytes()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "fixture packer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn fixture(bytes: Vec<u8>) -> Fixture {
    Fixture::new(bytes, "0.19.2", Channel::Stable, Platform::WindowsX64)
}

#[test]
fn real_packer_and_ed25519_verify_all_lanes_targets_retaining_exact_single_allocation() {
    for (lane, version) in [
        (Channel::Stable, "0.19.2"),
        (Channel::Rc, "0.19.2-rc.3"),
        (Channel::Nightly, "0.19.2-nightly.2026-10-03+01234567"),
    ] {
        for target in [Platform::MacArm64, Platform::MacX64, Platform::WindowsX64] {
            let bytes = packed(version, lane, target);
            let expected = bytes.clone();
            let pointer = bytes.as_ptr();
            let fixture = Fixture::new(bytes, version, lane, target);
            let (service, result) = fixture.verify().unwrap();
            assert_eq!(result.bytes, expected);
            assert_eq!(result.bytes.as_ptr(), pointer);
            assert_eq!(result.identity().unwrap().version, version);
            assert_eq!(result.archive_len().unwrap(), expected.len());
            assert_eq!(
                result.archive_sha256().unwrap(),
                <[u8; 32]>::from(Sha256::digest(expected))
            );
            assert_eq!(result.build_date().unwrap(), "2026-10-03");
            service.retire();
            assert_eq!(reject(result.identity()), SelectionError::Retired);
        }
    }
}
#[test]
fn actual_stored_deflated_descriptors_and_forced_zip64_local_headers_pass() {
    for config in [
        json!({"deflate":false}),
        json!({}),
        json!({"descriptor":true}),
        json!({"zip64":true}),
        json!({"zip64":true,"descriptor":true}),
    ] {
        let f = fixture(python_zip(config));
        let (_service, result) = f.verify().unwrap();
        assert!(result.archive_len().unwrap() > 0);
    }
}
#[test]
fn compiled_pin_is_exact_source_key_and_cannot_accept_controlled_fixture_signatures() {
    let key = Key::Compiled.get().unwrap();
    assert_eq!(STANDARD.encode(key.to_bytes()), PIN);
    let f = fixture(python_zip(json!({})));
    assert_eq!(
        reject(
            f.selected
                .verify_retained(f.archive, f.checksum, f.signature)
        ),
        SelectionError::Invalid
    );
}
#[test]
fn actual_signature_encoding_malleability_wrong_key_and_raw_message_tampering_reject() {
    let original = python_zip(json!({}));
    for attack in 0..7 {
        let mut f = fixture(original.clone());
        match attack {
            0 => {
                f.signature[10] ^= 1;
            }
            1 => {
                f.signature = STANDARD.encode([0u8; 64]).into_bytes();
            }
            2 => {
                f.signature = STANDARD
                    .encode(
                        SigningKey::from_bytes(&[0x26; 32])
                            .sign(&f.checksum)
                            .to_bytes(),
                    )
                    .into_bytes();
            }
            3 => {
                f.signature.push(b'\n');
            }
            4 => {
                f.signature = STANDARD.encode([0u8; 63]).into_bytes();
            }
            5 => {
                let mut raw = f.key.sign(&f.checksum).to_bytes();
                raw[63] |= 0xe0;
                f.signature = STANDARD.encode(raw).into_bytes();
            }
            _ => {
                f.checksum.push(b'\n');
                let c = f
                    .selected
                    .release
                    .files
                    .iter_mut()
                    .find(|f| f.name.ends_with(".txt"))
                    .unwrap();
                c.sha256 = Sha256::digest(&f.checksum).into();
                c.size_bytes = f.checksum.len() as u64;
            }
        }
        f.refresh_signature_declaration();
        assert_eq!(reject(f.verify()), SelectionError::Invalid);
    }
}
#[test]
fn signed_but_wrong_missing_duplicate_manifest_or_file_inventory_is_rejected() {
    for patch in [
        json!({"app":"other"}),
        json!({"channel":"rc"}),
        json!({"target":"aarch64-apple-darwin"}),
        json!({"platform":"macos"}),
        json!({"arch":"aarch64"}),
        json!({"version":"0.19.2-rc.1"}),
        json!({"target_version":"0.19.1"}),
        json!({"commit":"ab".repeat(20)}),
        json!({"build_date":"2026-02-30"}),
        json!({"files":["yield.exe"]}),
        json!({"files":["yield.exe","yield.exe","update-manifest.json"]}),
        json!({"files":["yield.exe","../update-manifest.json"]}),
    ] {
        assert_eq!(
            reject(fixture(python_zip(json!({"manifest":patch}))).verify()),
            SelectionError::Invalid
        );
    }
    for config in [
        json!({"remove_binary":true}),
        json!({"duplicate_manifest_field":true}),
    ] {
        assert_eq!(
            reject(fixture(python_zip(config)).verify()),
            SelectionError::Invalid
        );
    }
}
#[test]
fn genuine_zip_unsafe_members_unicode_case_and_implicit_directory_collisions_reject_entire_archive()
{
    for name in [
        "yield/../outside",
        "/yield/absolute",
        "yield/a//b",
        "yield/a\\b",
        "yield/a:b",
        "yield/./x",
        "yield/con.txt",
        "yield/A. ",
        "outside/file",
        "yield/update-manifest.json",
    ] {
        assert_eq!(
            reject(fixture(python_zip(json!({"extra":[[name,"other",0o100644]]}))).verify()),
            SelectionError::Invalid
        );
    }
    for extra in [
        json!([["yield/link", "target", 0o120777]]),
        json!([["yield/special", "other", 0o020600]]),
        json!([["yield/dir/", "", 0o100644]]),
        json!([
            ["yield/parent", "file", 0o100644],
            ["yield/parent/child", "child", 0o100644]
        ]),
        json!([["yield/YIELD.exe", "alias", 0o100644]]),
        json!([
            ["yield/caf\u{e9}", "one", 0o100644],
            ["yield/cafe\u{301}", "two", 0o100644]
        ]),
        json!([
            ["yield/stra\u{df}e", "one", 0o100644],
            ["yield/STRASSE", "two", 0o100644]
        ]),
        json!([
            ["yield/A/one", "one", 0o100644],
            ["yield/a/two", "two", 0o100644]
        ]),
    ] {
        assert_eq!(
            reject(fixture(python_zip(json!({"extra":extra}))).verify()),
            SelectionError::Invalid
        );
    }
}
#[test]
fn retired_or_cancelled_selection_cannot_verify_and_buffers_never_leave_into_filesystem() {
    let temp = tempfile::tempdir().unwrap();
    let paths = ["installed", "profile", "source"];
    for path in paths {
        std::fs::write(temp.path().join(path), b"untouched sentinel").unwrap();
    }
    for cancel in [false, true] {
        let f = fixture(python_zip(json!({})));
        if cancel {
            f.selected.lease.cancellation.cancel();
        } else {
            f.service.retire();
        }
        assert_eq!(reject(f.verify()), SelectionError::Retired);
    }
    let (_service, _result) = fixture(python_zip(json!({}))).verify().unwrap();
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 3);
    for path in paths {
        assert_eq!(
            std::fs::read(temp.path().join(path)).unwrap(),
            b"untouched sentinel"
        );
    }
}

#[test]
fn signed_checksum_inventory_requires_exact_complete_unique_raw_grammar_and_catalog_digests() {
    let archive = python_zip(json!({}));
    for attack in 0..6 {
        let mut f = fixture(archive.clone());
        let original = String::from_utf8(f.checksum.clone()).unwrap();
        let lines = original.lines().collect::<Vec<_>>();
        f.checksum = match attack {
            0 => format!("{}\n{}\n", lines[0], lines[1]).into_bytes(),
            1 => format!("{original}{}\n", lines[0]).into_bytes(),
            2 => original.replacen("  ", " ", 1).into_bytes(),
            3 => original
                .replacen(&lines[0][..64], &"00".repeat(32), 1)
                .into_bytes(),
            4 => format!("{original}\n").into_bytes(),
            _ => original.replacen(".zip", ".other", 1).into_bytes(),
        };
        f.signature = STANDARD
            .encode(f.key.sign(&f.checksum).to_bytes())
            .into_bytes();
        let file = f
            .selected
            .release
            .files
            .iter_mut()
            .find(|f| f.name.ends_with(".txt"))
            .unwrap();
        file.sha256 = Sha256::digest(&f.checksum).into();
        file.size_bytes = f.checksum.len() as u64;
        f.refresh_signature_declaration();
        assert_eq!(reject(f.verify()), SelectionError::Invalid);
    }
}

fn offset(bytes: &[u8], magic: &[u8]) -> usize {
    bytes.windows(magic.len()).position(|b| b == magic).unwrap()
}
fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn get16(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()))
}
fn get32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

#[test]
fn actual_zip64_end_records_and_unsigned_descriptors_preserve_supported_zip_semantics() {
    let original = python_zip(json!({"descriptor":true}));
    let descriptors = original
        .windows(4)
        .enumerate()
        .filter_map(|(i, b)| (b == b"PK\x07\x08").then_some(i))
        .collect::<Vec<_>>();
    assert_eq!(descriptors.len(), 2);
    let mut unsigned = original;
    for at in descriptors.iter().rev() {
        unsigned.drain(*at..*at + 4);
    }
    let central = offset(&unsigned, b"PK\x01\x02");
    let mut cursor = central;
    for _ in 0..2 {
        let local = get32(&unsigned, cursor + 42);
        let removed = descriptors.iter().filter(|i| **i < local as usize).count() as u32 * 4;
        put32(&mut unsigned, cursor + 42, local - removed);
        cursor += 46
            + get16(&unsigned, cursor + 28)
            + get16(&unsigned, cursor + 30)
            + get16(&unsigned, cursor + 32);
    }
    let end = offset(&unsigned, b"PK\x05\x06");
    put32(&mut unsigned, end + 16, central as u32);
    let (_service, _verified) = fixture(unsigned).verify().unwrap();

    let mut bytes = python_zip(json!({"zip64":true}));
    let end = offset(&bytes, b"PK\x05\x06");
    let count = get16(&bytes, end + 10) as u64;
    let size = u64::from(get32(&bytes, end + 12));
    let central = u64::from(get32(&bytes, end + 16));
    let mut record = Vec::new();
    record.extend_from_slice(&0x06064b50u32.to_le_bytes());
    record.extend_from_slice(&44u64.to_le_bytes());
    record.extend_from_slice(&45u16.to_le_bytes());
    record.extend_from_slice(&45u16.to_le_bytes());
    record.extend_from_slice(&[0; 8]);
    record.extend_from_slice(&count.to_le_bytes());
    record.extend_from_slice(&count.to_le_bytes());
    record.extend_from_slice(&size.to_le_bytes());
    record.extend_from_slice(&central.to_le_bytes());
    record.extend_from_slice(&0x07064b50u32.to_le_bytes());
    record.extend_from_slice(&[0; 4]);
    record.extend_from_slice(&(end as u64).to_le_bytes());
    record.extend_from_slice(&1u32.to_le_bytes());
    assert_eq!(record.len(), 76);
    bytes.splice(end..end, record);
    put16(&mut bytes, end + 76 + 8, u16::MAX);
    put16(&mut bytes, end + 76 + 10, u16::MAX);
    put32(&mut bytes, end + 76 + 12, u32::MAX);
    put32(&mut bytes, end + 76 + 16, u32::MAX);
    let (_service, _verified) = fixture(bytes.clone()).verify().unwrap();
    for at in [end + 32, end + 48, end + 64] {
        let mut bad = bytes.clone();
        bad[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(reject(fixture(bad).verify()), SelectionError::Invalid);
    }
}

#[test]
fn unsigned_descriptor_whose_real_crc_equals_optional_signature_is_not_misparsed() {
    // Solve the actual CRC32 linear map for a four-byte fixture; verify it with crc32fast.
    // This exercises a legal ZIP ambiguity with real CRC/hash/signature checks, not a fake CRC.
    let base = crc32fast::hash(&[0; 4]);
    let mut basis: [Option<(u32, u32)>; 32] = [None; 32];
    for bit in 0..32 {
        let mut value = crc32fast::hash(&(1u32 << bit).to_le_bytes()) ^ base;
        let mut patch = 1u32 << bit;
        while value != 0 {
            let index = (31 - value.leading_zeros()) as usize;
            if let Some((v, p)) = basis[index] {
                value ^= v;
                patch ^= p;
            } else {
                basis[index] = Some((value, patch));
                break;
            }
        }
    }
    let mut value = 0x08074b50 ^ base;
    let mut patch = 0u32;
    while value != 0 {
        let (v, p) = basis[(31 - value.leading_zeros()) as usize].unwrap();
        value ^= v;
        patch ^= p;
    }
    let binary = patch.to_le_bytes();
    assert_eq!(crc32fast::hash(&binary), 0x08074b50);
    let spelling = binary
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut bytes = python_zip(json!({"descriptor":true,"binary_hex":spelling}));
    let central = offset(&bytes, b"PK\x01\x02");
    let descriptor =
        30 + get16(&bytes, 26) + get16(&bytes, 28) + get32(&bytes, central + 20) as usize;
    assert_eq!(&bytes[descriptor..descriptor + 8], b"PK\x07\x08PK\x07\x08");
    bytes.drain(descriptor..descriptor + 4);
    let central = central - 4;
    let second = central
        + 46
        + get16(&bytes, central + 28)
        + get16(&bytes, central + 30)
        + get16(&bytes, central + 32);
    let local = get32(&bytes, second + 42);
    put32(&mut bytes, second + 42, local - 4);
    let end = offset(&bytes, b"PK\x05\x06");
    put32(&mut bytes, end + 16, central as u32);
    let (_service, _verified) = fixture(bytes).verify().unwrap();
}

#[test]
fn signed_actual_crc_length_local_agreement_encryption_offsets_counts_and_truncation_reject() {
    let original = python_zip(json!({"deflate":false}));
    let central = offset(&original, b"PK\x01\x02");
    let end = offset(&original, b"PK\x05\x06");
    for attack in 0..12 {
        let mut bytes = original.clone();
        match attack {
            0 => {
                let data = 30 + get16(&bytes, 26) + get16(&bytes, 28);
                bytes[data] ^= 1;
            } // actual member CRC after catalog/hash/signature agree
            1 => put32(&mut bytes, central + 24, 512 * 1024 * 1024 + 1),
            2 => put32(&mut bytes, central + 42, u32::MAX),
            3 => {
                put16(&mut bytes, end + 8, 10001);
                put16(&mut bytes, end + 10, 10001);
            }
            4 => put16(&mut bytes, central + 8, 1),
            5 => put16(&mut bytes, 6, 1),
            6 => put16(&mut bytes, 4, 45),
            7 => put16(&mut bytes, 10, 0x1234),
            8 => put32(&mut bytes, end + 12, 16 * 1024 * 1024 + 1),
            9 => put16(&mut bytes, end + 4, 1),
            10 => {
                bytes.truncate(bytes.len() - 1);
            }
            _ => {
                bytes.extend_from_slice(b"unsigned trailing garbage");
            }
        }
        assert_eq!(reject(fixture(bytes).verify()), SelectionError::Invalid);
    }
    let bomb =
        python_zip(json!({"extra":[["yield/highly-compressible","x".repeat(100000),0o100644]]}));
    assert_eq!(reject(fixture(bomb).verify()), SelectionError::Invalid);
    let descriptor = python_zip(json!({"descriptor":true}));
    let at = offset(&descriptor, b"PK\x07\x08");
    let mut bad = descriptor;
    bad[at + 4] ^= 1;
    assert_eq!(reject(fixture(bad).verify()), SelectionError::Invalid);
}

#[test]
fn actual_overallocated_archive_checksum_signature_and_expired_owner_deadline_reject() {
    let original = python_zip(json!({}));
    for attack in 0..3 {
        let mut f = fixture(original.clone());
        match attack {
            0 => {
                f.archive
                    .try_reserve_exact(ARCHIVE_CAP + 1 - f.archive.len())
                    .unwrap();
                assert!(f.archive.capacity() > ARCHIVE_CAP);
            }
            1 => {
                f.checksum
                    .try_reserve_exact(1024 * 1024 + 1 - f.checksum.len())
                    .unwrap();
                assert!(f.checksum.capacity() > 1024 * 1024);
            }
            _ => {
                f.signature
                    .try_reserve_exact(8193 - f.signature.len())
                    .unwrap();
                assert!(f.signature.capacity() > 8192);
            }
        }
        assert_eq!(reject(f.verify()), SelectionError::Invalid);
    }
    let f = fixture(original);
    assert_eq!(
        reject(verify_before(
            f.selected,
            f.archive,
            f.checksum,
            f.signature,
            Instant::now()
        )),
        SelectionError::Retired
    );
}

#[test]
fn combined_actual_central_local_extra_metadata_budget_and_component_limits_reject() {
    let members = (0..300)
        .map(|i| json!([format!("yield/f{i}"), "x", 0o100644]))
        .collect::<Vec<_>>();
    let bytes = python_zip(json!({"extra":members,"extra_padding":32760}));
    assert!(bytes.len() > 16 * 1024 * 1024);
    assert_eq!(reject(fixture(bytes).verify()), SelectionError::Invalid);
    for name in [
        format!("yield/{}", "x".repeat(256)),
        format!("yield/{}/x", vec!["a"; 32].join("/")),
        "yield/com¹.txt".into(),
        "yield/conin$".into(),
        "yield/a\u{200d}b".into(),
    ] {
        assert_eq!(
            reject(fixture(python_zip(json!({"extra":[[name,"x",0o100644]]}))).verify()),
            SelectionError::Invalid
        );
    }
    // Ordinary Unicode remains valid, without panicking at a multibyte reserved-name boundary.
    let (_service, _bytes) = fixture(python_zip(json!({"extra":[["yield/xxé","x",0o100644]]})))
        .verify()
        .unwrap();
}

#[test]
fn manifest_uses_its_real_one_mib_limit_and_deflate_cannot_hide_trailing_compressed_bytes() {
    let padding = (0..1100)
        .map(|i| {
            (0..16)
                .map(|j| hex(Sha256::digest(format!("bounded manifest fixture {i}/{j}")).into()))
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let large = python_zip(json!({"manifest":{"future_metadata":&padding[..700]}}));
    let (_service, _verified) = fixture(large).verify().unwrap();
    let oversized = python_zip(json!({"manifest":{"future_metadata":padding}}));
    assert_eq!(reject(fixture(oversized).verify()), SelectionError::Invalid);

    let mut bytes = python_zip(json!({}));
    let central = offset(&bytes, b"PK\x01\x02");
    let compressed = get32(&bytes, central + 20);
    let end = 30 + get16(&bytes, 26) + get16(&bytes, 28) + compressed as usize;
    bytes.insert(end, b'x');
    let central = central + 1;
    put32(&mut bytes, 18, compressed + 1);
    put32(&mut bytes, central + 20, compressed + 1);
    let second = central
        + 46
        + get16(&bytes, central + 28)
        + get16(&bytes, central + 30)
        + get16(&bytes, central + 32);
    let local = get32(&bytes, second + 42);
    put32(&mut bytes, second + 42, local + 1);
    let eocd = offset(&bytes, b"PK\x05\x06");
    put32(&mut bytes, eocd + 16, central as u32);
    assert_eq!(reject(fixture(bytes).verify()), SelectionError::Invalid);
}
