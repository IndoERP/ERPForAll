//! Private connection handoff shared by installed native and component browser fixtures.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

pub fn private(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

pub fn run(dir: &Path, mut contract: serde_json::Value) {
    let driver = std::env::var_os("ERP_WIZARD_BROWSER_DRIVER")
        .expect("installed browser fixture requires ERP_WIZARD_BROWSER_DRIVER");
    // scripts/sync.sh deliberately excludes .git and stamps the copied source instead.
    // Dirty identities must never certify browser assets or an acceptance result.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let revision = fs::read_to_string(root.join("REVISION"))
        .expect("browser lane requires scripts/sync.sh's REVISION stamp");
    let revision = revision.trim();
    assert!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "browser lane requires a clean 40-hex REVISION stamp"
    );
    let digest = Command::new("shasum")
        .args(["-a", "256"])
        .arg(dir.join("manifest.json"))
        .output()
        .unwrap();
    assert!(digest.status.success());
    contract["sourceRevision"] = revision.into();
    contract["assetsRevision"] = std::env::var("ERP_WIZARD_ASSETS_REVISION")
        .expect("browser lane requires ERP_WIZARD_ASSETS_REVISION")
        .into();
    contract["assetsManifestSha256"] = std::str::from_utf8(&digest.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .into();
    let path = dir.join("wizard-browser.json");
    private(&path, &serde_json::to_string(&contract).unwrap());
    let mut child = Command::new("node").arg(driver).arg(&path).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "installed wizard browser acceptance failed"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("installed wizard browser acceptance exceeded its 180s admission deadline");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
