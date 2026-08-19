use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use wren_extension::{EXTENSION_API_VERSION, HostPlacement, HostRequest, HostResponse};

fn exercise_host(executable: &str, expected_placement: HostPlacement) {
    let mut child = Command::new(executable).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("spawn extension host");
    let mut input = child.stdin.take().expect("stdin");
    let output = child.stdout.take().expect("stdout");
    serde_json::to_writer(&mut input, &HostRequest::Hello).expect("hello request");
    input.write_all(b"\n").expect("newline");
    input.flush().expect("flush");
    let mut reader = BufReader::new(output);
    let mut line = String::new();
    reader.read_line(&mut line).expect("response");
    let response: HostResponse = serde_json::from_str(&line).expect("hello response");
    assert_eq!(response, HostResponse::Hello { api_version: EXTENSION_API_VERSION.into(), placement: expected_placement });
    serde_json::to_writer(&mut input, &HostRequest::Shutdown).expect("shutdown request");
    input.write_all(b"\n").expect("newline");
    input.flush().expect("flush");
    assert!(child.wait().expect("wait").success());
}

#[test]
fn client_and_workspace_host_binaries_are_placement_faithful() {
    exercise_host(env!("CARGO_BIN_EXE_wren-client-extension-host"), HostPlacement::Client);
    exercise_host(env!("CARGO_BIN_EXE_wren-workspace-extension-host"), HostPlacement::Workspace);
}
