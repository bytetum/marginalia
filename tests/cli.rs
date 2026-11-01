//! The compiled binary's command line. Cargo builds the binary for integration tests and tells us where it is.

use std::process::Command;

#[test]
fn version_flag_prints_the_baked_in_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_marginalia")).arg("--version").output().expect("run marginalia");
    assert!(out.status.success(), "{out:?}");
    // clap prints `<name> <version>`; release builds bake MARGINALIA_VERSION in, local builds report Cargo's.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("marginalia {}", marginalia::VERSION));
}

#[test]
fn version_falls_back_to_the_cargo_version() {
    if option_env!("MARGINALIA_VERSION").is_none() {
        assert_eq!(marginalia::VERSION, env!("CARGO_PKG_VERSION"));
    }
}

/// `serve --socket` listens on a Unix socket, trusts `--tailscale-login` there, and cleans up on SIGTERM.
#[cfg(unix)]
#[test]
fn serve_on_a_unix_socket_with_a_trusted_tailscale_login() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    fn get(sock: &std::path::Path, path: &str, extra: &str) -> String {
        let mut stream = UnixStream::connect(sock).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
        write!(stream, "GET {path} HTTP/1.1\r\nHost: marginalia\r\nConnection: close\r\n{extra}\r\n").expect("write");
        let mut reply = String::new();
        stream.read_to_string(&mut reply).expect("read");
        reply
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("m.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_marginalia"))
        .current_dir(dir.path()) // no stray .env
        .env_clear()
        .env("MARGINALIA_DATA", dir.path().join("data"))
        .env("MARGINALIA_TOKEN", "s3cret")
        .args(["serve", "--socket"])
        .arg(&sock)
        .args(["--tailscale-login", "owner@github"])
        .spawn()
        .expect("spawn marginalia");

    let start = Instant::now();
    while UnixStream::connect(&sock).is_err() {
        assert!(start.elapsed() < Duration::from_secs(60), "socket never came up");
        assert!(child.try_wait().expect("wait").is_none(), "marginalia exited early");
        std::thread::sleep(Duration::from_millis(100));
    }

    let health = get(&sock, "/health", "");
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.contains(r#""ok":true"#), "{health}");

    assert!(get(&sock, "/me", "").starts_with("HTTP/1.1 401"));
    let me = get(&sock, "/me", "Tailscale-User-Login: Owner@GitHub\r\n");
    assert!(me.starts_with("HTTP/1.1 200") && me.contains(r#""auth":"tailscale""#), "{me}");
    assert!(get(&sock, "/me", "Tailscale-User-Login: intruder@github\r\n").starts_with("HTTP/1.1 401"));
    let me = get(&sock, "/me", "Authorization: Bearer s3cret\r\n");
    assert!(me.starts_with("HTTP/1.1 200") && me.contains(r#""auth":"token""#), "{me}");

    let killed = Command::new("kill").args(["-TERM", &child.id().to_string()]).status().expect("kill");
    assert!(killed.success());
    let status = child.wait().expect("wait");
    assert!(status.success(), "{status:?}");
    assert!(!sock.exists(), "the socket file is removed on a clean exit");
}

#[cfg(unix)]
#[test]
fn socket_and_bind_together_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_marginalia"))
        .current_dir(dir.path())
        .env_clear()
        .env("MARGINALIA_DATA", dir.path().join("data"))
        .args(["serve", "--bind", "127.0.0.1:0", "--socket"])
        .arg(dir.path().join("m.sock"))
        .output()
        .expect("run marginalia");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not both"), "{out:?}");
}
