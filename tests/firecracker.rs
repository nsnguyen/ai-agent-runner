// The Firecracker API client against a fake API socket: no KVM needed.
mod common;

use common::{FakeApi, script};
use runner::firecracker::{self, Api, BOOT_ARGS, Error, Guest};
use runner::net::DEFAULT_SUBNET;
use serde_json::json;
use std::path::Path;

#[test]
fn put_sends_json_and_takes_a_204() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let api = FakeApi::serve(&sock, |_| (204, None));
    let body = json!({"vcpu_count": 2, "mem_size_mib": 2048});
    Api::new(&sock).put("/machine-config", &body).unwrap();

    let reqs = api.requests();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(
        (req.method.as_str(), req.path.as_str()),
        ("PUT", "/machine-config")
    );
    assert_eq!(req.header("content-type"), Some("application/json"));
    assert_eq!(req.body, body);
}

#[test]
fn a_fault_comes_back_with_firecrackers_message() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let fault = json!({"fault_message": "The kernel file cannot be opened"}).to_string();
    let _api = FakeApi::serve(&sock, move |_| (400, Some(fault.clone())));
    let err = Api::new(&sock).put("/boot-source", &json!({})).unwrap_err();
    assert!(
        matches!(&err, Error::Api { status: 400, message, .. } if message == "The kernel file cannot be opened"),
        "{err}"
    );
    assert!(err.to_string().contains("PUT /boot-source"), "{err}");
}

#[test]
fn a_reply_with_a_body_is_read_to_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let _api = FakeApi::serve(&sock, |_| (200, Some(r#"{"state":"Running"}"#.into())));
    // Firecracker keeps the connection open, so a client that read to EOF
    // would hang here.
    let api = Api::new(&sock);
    api.put("/actions", &json!({})).unwrap();
    api.put("/actions", &json!({})).unwrap();
}

#[test]
fn no_socket_means_nothing_is_listening() {
    let dir = tempfile::tempdir().unwrap();
    let err = Api::new(dir.path().join("api.sock"))
        .put("/actions", &json!({}))
        .unwrap_err();
    assert!(matches!(err, Error::NotListening(_)), "{err}");

    // A socket file left by a Firecracker that died.
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    let err = Api::new(&stale).put("/actions", &json!({})).unwrap_err();
    assert!(matches!(err, Error::NotListening(_)), "{err}");
}

#[test]
fn configure_then_start_is_the_whole_boot() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let fake = FakeApi::serve(&sock, |_| (204, None));
    let api = Api::new(&sock);
    let net = DEFAULT_SUBNET.net(1).unwrap();
    let ip_arg = "ip=172.16.0.6::172.16.0.5:255.255.255.252:dev:eth0:off:1.1.1.1";
    let guest = Guest {
        kernel: Path::new("/k/vmlinux"),
        disk: Path::new("/vms/dev/gens/1.ext4"),
        cpus: 4,
        mem_mib: 4096,
        vsock: Path::new("/vms/dev/vsock.sock"),
        net: &net,
        ip_arg,
    };
    api.configure(&guest).unwrap();
    api.start().unwrap();
    api.ctrl_alt_del().unwrap();
    let call = |p: &str, b| (p.to_string(), b);
    assert_eq!(
        fake.calls(),
        [
            call(
                "PUT /boot-source",
                json!({"kernel_image_path": "/k/vmlinux",
                       "boot_args": format!("{BOOT_ARGS} {ip_arg}")})
            ),
            call(
                "PUT /drives/rootfs",
                json!({"drive_id": "rootfs", "path_on_host": "/vms/dev/gens/1.ext4",
                       "is_root_device": true, "is_read_only": false})
            ),
            call(
                "PUT /machine-config",
                json!({"vcpu_count": 4, "mem_size_mib": 4096})
            ),
            call(
                "PUT /vsock",
                json!({"guest_cid": 3, "uds_path": "/vms/dev/vsock.sock"})
            ),
            call(
                "PUT /network-interfaces/eth0",
                json!({"iface_id": "eth0", "host_dev_name": "fcac100004",
                       "guest_mac": "06:00:ac:10:00:06"})
            ),
            call("PUT /actions", json!({"action_type": "InstanceStart"})),
            call("PUT /actions", json!({"action_type": "SendCtrlAltDel"})),
        ]
    );
}

#[test]
fn the_console_is_on_serial_and_ctrl_alt_del_can_reach_the_guest() {
    assert!(BOOT_ARGS.contains("console=ttyS0"));
    // i8042 carries Ctrl-Alt-Del; Firecracker's own defaults turn the serial port off.
    assert!(!BOOT_ARGS.contains("i8042.nokbd"));
    assert!(!BOOT_ARGS.contains("8250.nr_uarts=0"));
    // A panicked guest hangs rather than reboots, so its exit can't pass for a clean one.
    assert!(!BOOT_ARGS.contains("panic="));
}

#[test]
fn the_version_is_pinned_in_one_file() {
    let pinned = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/host/firecracker-version"
    ))
    .unwrap();
    assert_eq!(firecracker::VERSION, pinned.trim());
    let digits = firecracker::VERSION.strip_prefix('v').unwrap();
    assert_eq!(
        digits
            .split('.')
            .filter(|p| p.parse::<u32>().is_ok())
            .count(),
        3
    );
}

#[test]
fn only_the_pinned_version_passes() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("firecracker");
    let pinned = firecracker::VERSION;
    script(&bin, &format!("printf 'Firecracker {pinned}\\n\\n'\n"));
    firecracker::check_version(&bin).unwrap();

    script(&bin, "printf 'Firecracker v1.0.0\\n\\n'\n");
    let err = firecracker::check_version(&bin).unwrap_err();
    assert!(
        matches!(&err, Error::Version { found, .. } if found == "v1.0.0"),
        "{err}"
    );
    assert!(err.to_string().contains(pinned), "{err}");

    let err = firecracker::check_version(&dir.path().join("missing")).unwrap_err();
    assert!(matches!(err, Error::Run { .. }), "{err}");
}
