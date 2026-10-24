//! D-Bus activation against a private bus. The outer test starts
//! `dbus-daemon` (from the dev shell) with its own configuration, then runs
//! this test binary again with `DBUS_SESSION_BUS_ADDRESS` pointing at that
//! bus. The library reads the session bus from the environment. The inner
//! test serves `org.freedesktop.Application` and launches through the crate.
//! The bus listens on a Linux abstract socket, so no path of the temporary
//! folder goes into its address.

#![cfg(all(feature = "dbus", target_os = "linux"))]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use xdg_kit_desktop_entry::{DesktopEntry, Launch, LaunchOptions, Target, prepare_launch};
use zbus::OwnedVariant;

#[test]
fn activation_calls_reach_the_application() {
    let dir = tempfile::tempdir().unwrap();
    // Unique to this run: the process and the time.
    let address = format!(
        "unix:abstract=xdg-kit-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // A configuration of its own: `--session` reads /etc/dbus-1, which the
    // Nix build sandbox does not have.
    let config = dir.path().join("bus.conf");
    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
        ),
    )
    .unwrap();
    let mut child = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--nopidfile", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon is not on PATH: run the tests in the dev shell (nix develop)");
    let stdout = child.stdout.take().unwrap();
    let _daemon = Daemon(child);
    // The address is printed once the bus listens; nothing, if it exited.
    let mut printed = String::new();
    BufReader::new(stdout).read_line(&mut printed).unwrap();
    assert!(
        !printed.is_empty(),
        "dbus-daemon did not start (its message is above): {address}"
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "served_activation",
            "--ignored",
            "--test-threads=1",
        ])
        .env("DBUS_SESSION_BUS_ADDRESS", &address)
        .env(PRIVATE_BUS, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the inner test failed:\n{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A filter that matches nothing passes too: the inner test must have run.
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "the inner test did not run:\n{stdout}"
    );
}

/// Stops the bus however the test ends: a `Child` left running would keep
/// the test's output open.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Set by the outer test, so the inner one never runs on the real session
/// bus (as with `cargo test -- --include-ignored`).
const PRIVATE_BUS: &str = "XDG_KIT_TEST_PRIVATE_BUS";

/// What the served application was asked to do.
#[derive(Default)]
struct Calls(Arc<Mutex<Vec<String>>>);

/// The platform data the launch passes on.
fn token(platform_data: &HashMap<String, OwnedVariant>) -> String {
    let get = |key: &str| {
        platform_data
            .get(key)
            .and_then(|value| String::try_from(value.clone()).ok())
            .unwrap_or_default()
    };
    format!(
        "{} startup={}",
        get("activation-token"),
        get("desktop-startup-id")
    )
}

// zbus hands the decoded arguments over by value.
#[allow(clippy::needless_pass_by_value)]
#[zbus::interface(name = "org.freedesktop.Application")]
impl Calls {
    fn activate(&self, platform_data: HashMap<String, OwnedVariant>) {
        self.0
            .lock()
            .unwrap()
            .push(format!("Activate token={}", token(&platform_data)));
    }

    fn open(&self, uris: Vec<String>, platform_data: HashMap<String, OwnedVariant>) {
        self.0.lock().unwrap().push(format!(
            "Open {} token={}",
            uris.join(" "),
            token(&platform_data)
        ));
    }

    fn activate_action(
        &self,
        name: String,
        parameter: Vec<OwnedVariant>,
        platform_data: HashMap<String, OwnedVariant>,
    ) {
        self.0.lock().unwrap().push(format!(
            "ActivateAction {name} parameters={} token={}",
            parameter.len(),
            token(&platform_data)
        ));
    }
}

#[test]
#[ignore = "run by activation_calls_reach_the_application, on a private bus"]
fn served_activation() {
    if std::env::var_os(PRIVATE_BUS).is_none() {
        eprintln!(
            "skipped: runs only on the private bus of activation_calls_reach_the_application"
        );
        return;
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    // A connection is served only while a thread runs zbus's `block_on`, so
    // the application is served from a thread that stays in it; it ends
    // with the test process.
    let (served, ready) = std::sync::mpsc::channel();
    let served_calls = Calls(Arc::clone(&calls));
    std::thread::spawn(move || {
        zbus::block_on(async move {
            // A '-' in the name becomes '_' in the object path.
            let _server = zbus::connection::Builder::session()
                .name("org.example.Foo-Bar")
                .serve_at("/org/example/Foo_Bar", served_calls)
                .build()
                .await
                .unwrap();
            served.send(()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    ready.recv().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("org.example.Foo-Bar.desktop");
    std::fs::write(
        &path,
        "[Desktop Entry]\nType=Application\nName=Foo\nExec=foo %U\nDBusActivatable=true\n\
         Actions=New;\n[Desktop Action New]\nName=New\nExec=foo --new\n",
    )
    .unwrap();
    let entry = DesktopEntry::read(&path).unwrap();
    let mut options = LaunchOptions::default();
    options.activation_token = Some("tok".into());
    options.startup_id = Some("sid".into());
    let launch = |targets: &[Target], options: &LaunchOptions| {
        let plan = prepare_launch(&entry, targets, options).unwrap();
        assert!(matches!(plan.launch, Launch::DBus(_)), "{plan:?}");
        assert!(plan.spawn().unwrap().is_empty());
    };
    // The first launch warms persistent state up — zbus's runtime and
    // zruntime's reactor open descriptors of their own on first use — so
    // that the count below sees only per-call descriptors.
    launch(&[], &options);
    let baseline = open_sockets();
    launch(&[], &options);
    launch(
        &[
            Target::Path("/tmp/a b".into()),
            Target::Uri("https://example.org/x".into()),
        ],
        &options,
    );
    options.action = Some("New".into());
    launch(&[], &options);
    // Teardown is part of each call: no socket accumulates across calls.
    // (The serving thread drives the shared runtime here, so even the one
    // descriptor the aborted reader may briefly hold is reaped; a client
    // whose runtime nothing drives keeps that one until its next call.)
    assert_eq!(open_sockets(), baseline);
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "Activate token=tok startup=sid",
            "Activate token=tok startup=sid",
            "Open file:///tmp/a%20b https://example.org/x token=tok startup=sid",
            "ActivateAction New parameters=0 token=tok startup=sid"
        ]
    );
}

/// How many sockets this process has open. Deterministic here: the inner
/// test runs alone in its own process (`--test-threads=1`), and only
/// connections hold sockets — the runtimes' own descriptors (zruntime's
/// reactors come and go with its helper thread) are left out.
fn open_sockets() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter(|entry| {
            std::fs::read_link(entry.as_ref().unwrap().path())
                .is_ok_and(|target| target.to_string_lossy().starts_with("socket:"))
        })
        .count()
}
