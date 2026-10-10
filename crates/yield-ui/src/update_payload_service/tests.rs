//! Controlled genuine crypto/ZIP fixtures, never production/native qualification.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
pub(crate) static OWNER_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn responses(version: &str, lane: Channel, wrong_manifest: bool) -> Vec<Vec<u8>> {
    let script = r#"
import io,json,sys,zipfile,stat
version,channel,wrong=sys.argv[1:]
manifest={'app':'yield','version':version if wrong=='false' else '0.19.2-rc.99','target_version':'0.19.2','channel':channel,'target':'x86_64-pc-windows-msvc','platform':'windows','arch':'x86_64','commit':'0123456789abcdef0123456789abcdef01234567','build_date':'2026-10-03','files':['yield.exe','update-manifest.json']}
out=io.BytesIO()
with zipfile.ZipFile(out,'w',compression=zipfile.ZIP_DEFLATED) as z:
 for name,data in [('yield/yield.exe',b'controlled nonNative binary'),('yield/update-manifest.json',json.dumps(manifest).encode())]:
  info=zipfile.ZipInfo(name);info.create_system=3;info.external_attr=(stat.S_IFREG|0o755)<<16;info.compress_type=zipfile.ZIP_DEFLATED;z.writestr(info,data)
sys.stdout.buffer.write(out.getvalue())
"#;
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            script,
            version,
            lane_name(lane),
            if wrong_manifest { "true" } else { "false" },
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let archive = output.stdout;
    let hash = hex(&Sha256::digest(&archive));
    let version_parsed = semver::Version::parse(version).unwrap();
    let contract: toml::Value = toml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../release_contract.toml"
    )))
    .unwrap();
    let name = |template: &str, os: &str, arch: &str| {
        template
            .replace("{APP_NAME}", contract["app_name"].as_str().unwrap())
            .replace("{version}", "0.19.2")
            .replace(
                "{rc_number}",
                version_parsed
                    .pre
                    .as_str()
                    .strip_prefix("rc.")
                    .unwrap_or(""),
            )
            .replace("{platform}", os)
            .replace("{arch}", arch)
    };
    let template = contract["templates"][format!("{}_asset", lane_name(lane))]
        .as_str()
        .unwrap();
    let names = [
        ("macos", "aarch64"),
        ("macos", "x86_64"),
        ("windows", "x86_64"),
    ]
    .map(|(os, arch)| name(template, os, arch));
    let checksum = names
        .iter()
        .map(|name| format!("{hash}  {name}\n"))
        .collect::<String>()
        .into_bytes();
    let signature = STANDARD
        .encode(
            SigningKey::from_bytes(&[0x25; 32])
                .sign(&checksum)
                .to_bytes(),
        )
        .into_bytes();
    let checksum_name = name(
        contract["templates"][format!("{}_checksums", lane_name(lane))]
            .as_str()
            .unwrap(),
        "",
        "",
    );
    let mut files=names.iter().map(|name|json!({"name":name,"size_bytes":archive.len(),"sha256":hash,"url":format!("/yield/assets/{name}")})).collect::<Vec<_>>();
    for (name, bytes) in [
        (checksum_name.clone(), &checksum),
        (format!("{checksum_name}.sig"), &signature),
    ] {
        files.push(json!({"name":name,"size_bytes":bytes.len(),"sha256":hex(&Sha256::digest(bytes)),"url":format!("/yield/assets/{name}")}));
    }
    let build_id = format!("yield-{version}");
    let catalog=serde_json::to_vec(&json!({"releases":[{"build_id":build_id,"version":version,"build_number":8,"commit":COMMIT,"channel":lane_name(lane),"files":files}]})).unwrap();
    let gate =
        serde_json::to_vec(&json!({"build_id":build_id,"download_token":"controlled GPUI token"}))
            .unwrap();
    [
        (&catalog, "application/json"),
        (&gate, "application/json"),
        (&checksum, "text/plain"),
        (&signature, "application/pgp-signature"),
        (&archive, "application/zip"),
    ]
    .into_iter()
    .map(|(bytes, mime)| http(bytes, mime))
    .collect()
}
pub(crate) fn http(bytes: &[u8], mime: &str) -> Vec<u8> {
    let mut response=format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).into_bytes();
    response.extend_from_slice(bytes);
    response
}
fn service(address: std::net::SocketAddr, lane: Channel) -> UpdatePayloadService {
    let owner = PayloadStagingService::for_loopback_tests(
        address,
        "0.19.1",
        7,
        [0xab; 20],
        Platform::WindowsX64,
    )
    .unwrap();
    UpdatePayloadService {
        context: Arc::new(PayloadContext {
            owner,
            installed: InstalledPayload {
                full_version: "0.19.1".into(),
                number: 7,
                commit: [0xab; 20],
                platform: Platform::WindowsX64,
            },
        }),
        channel: lane,
    }
}
pub(crate) fn request(socket: &mut TcpStream) -> String {
    // Accepted sockets can inherit a nonblocking listener's mode on Darwin.
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    while !bytes.ends_with(b"\r\n\r\n") {
        let n = socket.read(&mut chunk).unwrap();
        assert!(n > 0 && bytes.len() + n < 24 * 1024);
        bytes.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(bytes).unwrap()
}
pub(crate) fn serve(
    responses: Vec<Vec<u8>>,
    lane: Channel,
) -> (UpdatePayloadService, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let service = service(listener.local_addr().unwrap(), lane);
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(s) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "missing actual GPUI owner request"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            requests.push(request(&mut socket));
            let _ = socket.write_all(&response);
        }
        requests
    });
    (service, handle)
}
pub(crate) fn blocked(
    response: Vec<u8>,
    lane: Channel,
) -> (
    UpdatePayloadService,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let service = service(listener.local_addr().unwrap(), lane);
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        request(&mut socket);
        let split = response.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
        socket.write_all(&response[..split]).unwrap();
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let _ = socket.write_all(&response[split..]);
    });
    (service, entered, release, server)
}
#[gpui::test]
fn compiled_payload_registration_is_absent_on_unsupported_host_and_replacement_retires_exact_owner(
    cx: &mut gpui::TestAppContext,
) {
    let _guard = OWNER_TEST.lock().unwrap();
    cx.update(|app| {
        register(app);
        if ReleaseBuildIdentity::compiled().is_err()
            || !cfg!(any(target_os = "macos", target_os = "windows"))
        {
            assert!(app.try_global::<UpdatePayloadService>().is_none());
        }
    });
    let (service, server) = serve(Vec::new(), Channel::Stable);
    server.join().unwrap();
    let old = service.clone();
    cx.update(|app| {
        replace(app, Some(service));
        replace(app, None);
        assert!(app.try_global::<UpdatePayloadService>().is_none());
    });
    assert!(matches!(
        old.context.owner.start(
            Channel::Stable,
            yield_platform::payload_selection::SelectionCancellation::default()
        ),
        Err(yield_platform::payload_selection::SelectionError::Retired)
    ));
}

pub(crate) fn install(app: &mut gpui::App, service: UpdatePayloadService) {
    replace(app, Some(service));
}

fn lane_name(lane: Channel) -> &'static str {
    match lane {
        Channel::Stable => "stable",
        Channel::Rc => "rc",
        Channel::Nightly => "nightly",
    }
}
