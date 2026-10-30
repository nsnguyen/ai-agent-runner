// The host-guest wire protocol on byte buffers: frames, hellos and limits.
use proptest::prelude::*;
use runner::proto::{self, Error, Exit, FromGuest, MAX_FRAME, Request, ToGuest, VERSION, WinSize};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use std::io::Cursor;

fn win() -> impl Strategy<Value = WinSize> {
    (any::<u16>(), any::<u16>()).prop_map(|(rows, cols)| WinSize { rows, cols })
}

fn bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..512)
}

fn request() -> impl Strategy<Value = Request> {
    let strings = || prop::collection::vec(".*", 0..4);
    (
        strings(),
        prop::collection::vec((".*", ".*"), 0..4),
        prop::option::of(".*"),
        prop::option::of(".*"),
        prop::option::of(win()),
    )
        .prop_map(|(argv, env, cwd, user, tty)| Request {
            argv,
            env,
            cwd,
            user,
            tty,
        })
}

fn to_guest() -> impl Strategy<Value = ToGuest> {
    prop_oneof![
        bytes().prop_map(ToGuest::Stdin),
        Just(ToGuest::CloseStdin),
        win().prop_map(ToGuest::Resize),
        any::<i32>().prop_map(ToGuest::Signal),
    ]
}

fn from_guest() -> impl Strategy<Value = FromGuest> {
    prop_oneof![
        bytes().prop_map(FromGuest::Stdout),
        bytes().prop_map(FromGuest::Stderr),
        any::<i32>().prop_map(|c| FromGuest::Exit(Exit::Code(c))),
        any::<i32>().prop_map(|s| FromGuest::Exit(Exit::Signal(s))),
        ".*".prop_map(FromGuest::Error),
    ]
}

fn encode<T: Serialize>(msgs: &[T]) -> Vec<u8> {
    let mut buf = Vec::new();
    for m in msgs {
        proto::write_frame(&mut buf, m).unwrap();
    }
    buf
}

fn decode_all<T: DeserializeOwned>(buf: &[u8]) -> Vec<T> {
    let mut r = Cursor::new(buf);
    let mut out = Vec::new();
    while let Some(m) = proto::read_frame(&mut r).unwrap() {
        out.push(m);
    }
    out
}

fn round_trips<T: Serialize + DeserializeOwned + PartialEq + Debug>(msgs: Vec<T>) {
    assert_eq!(decode_all::<T>(&encode(&msgs)), msgs);
}

proptest! {
    #[test]
    fn requests_round_trip(msgs in prop::collection::vec(request(), 0..4)) {
        round_trips(msgs);
    }

    #[test]
    fn host_frames_round_trip(msgs in prop::collection::vec(to_guest(), 0..8)) {
        round_trips(msgs);
    }

    #[test]
    fn guest_frames_round_trip(msgs in prop::collection::vec(from_guest(), 0..8)) {
        round_trips(msgs);
    }

    // Cut anywhere inside a frame, a read fails; it never returns a frame
    // or mistakes the cut for a clean close.
    #[test]
    fn truncated_frames_are_refused(msg in from_guest(), cut in any::<prop::sample::Index>()) {
        let buf = encode(&[msg]);
        let cut = 1 + cut.index(buf.len() - 1);
        let got = proto::read_frame::<FromGuest>(&mut Cursor::new(&buf[..cut]));
        prop_assert!(matches!(got, Err(Error::Truncated)), "{got:?}");
    }

    // An oversized length is refused from the header alone: nothing is
    // read or allocated for the body.
    #[test]
    fn oversized_frames_are_refused(len in (MAX_FRAME as u32 + 1)..=u32::MAX) {
        let got = proto::read_frame::<ToGuest>(&mut Cursor::new(len.to_be_bytes()));
        prop_assert!(matches!(got, Err(Error::TooBig(n)) if n == len as usize), "{got:?}");
    }

    #[test]
    fn garbage_is_an_error_not_a_panic(buf in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = proto::read_frame::<Request>(&mut Cursor::new(&buf));
        let _ = proto::read_frame::<ToGuest>(&mut Cursor::new(&buf));
    }
}

#[test]
fn a_clean_close_between_frames_is_none() {
    let got = proto::read_frame::<ToGuest>(&mut Cursor::new(Vec::new())).unwrap();
    assert_eq!(got, None);
}

#[test]
fn an_oversized_frame_is_not_written() {
    let mut buf = Vec::new();
    let got = proto::write_frame(&mut buf, &FromGuest::Stdout(vec![0; MAX_FRAME]));
    assert!(matches!(got, Err(Error::TooBig(_))), "{got:?}");
    assert!(buf.is_empty());
}

#[test]
fn a_frame_with_bytes_left_over_is_refused() {
    let mut buf = encode(&[ToGuest::CloseStdin]);
    buf[3] += 1;
    buf.push(0);
    let got = proto::read_frame::<ToGuest>(&mut Cursor::new(buf));
    assert!(matches!(got, Err(Error::Decode(_))), "{got:?}");
}

#[test]
fn matching_hellos_pass() {
    let mut buf = Vec::new();
    proto::hello(&mut buf).unwrap();
    proto::check_hello(&mut Cursor::new(buf)).unwrap();
}

#[test]
fn a_mismatched_version_names_both() {
    let mut buf = Vec::new();
    proto::hello(&mut buf).unwrap();
    let at = buf.len() - 4;
    buf[at..].copy_from_slice(&(VERSION + 1).to_be_bytes());
    let err = proto::check_hello(&mut Cursor::new(buf)).unwrap_err();
    let want = format!(
        "the other end speaks protocol v{}, this end speaks v{VERSION}",
        VERSION + 1
    );
    assert_eq!(err.to_string(), want);
}

#[test]
fn something_else_on_the_port_is_not_a_guest() {
    let err = proto::check_hello(&mut Cursor::new(b"SSH-2.0-OpenSSH".to_vec())).unwrap_err();
    assert!(matches!(err, Error::NotPeer), "{err:?}");
}
