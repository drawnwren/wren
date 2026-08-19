use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, bail};
use rmpv::Value;
use rmpv::decode::read_value;
use rmpv::encode::write_value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleState {
    pub mode: String,
    pub pending_operator: String,
    pub buffer: Vec<String>,
    pub cursor: serde_json::Value,
    pub selections: serde_json::Value,
    pub registers: serde_json::Value,
    pub marks: serde_json::Value,
    pub jumplist: serde_json::Value,
    pub changelist: serde_json::Value,
    pub search: serde_json::Value,
    pub messages: String,
    pub undo_tree: serde_json::Value,
    pub options: serde_json::Value,
}

pub struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_message_id: u64,
    version: String,
}

impl Oracle {
    pub fn spawn() -> Result<Self> {
        let mut child = Command::new("nvim")
            .args(["--embed", "--headless", "--clean", "-n", "-u", "NONE", "-i", "NONE"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn pinned nvim --embed --headless")?;
        let input = child.stdin.take().context("capture nvim stdin")?;
        let output = child.stdout.take().context("capture nvim stdout")?;
        let mut oracle = Self { child, input, output: BufReader::new(output), next_message_id: 1, version: String::new() };
        let api_info = oracle.request("nvim_get_api_info", Vec::new())?;
        oracle.version = parse_version(&api_info)?;
        oracle.request("nvim_command", vec![Value::from("set shortmess+=I noswapfile undolevels=1000 nohlsearch")])?;
        Ok(oracle)
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn reset(&mut self, lines: &[&str]) -> Result<()> {
        self.request(
            "nvim_command",
            vec![Value::from("silent! delmarks! | silent! delmarks A-Z0-9 | clearjumps | let @/ = '' | messages clear | setlocal undolevels=-1")],
        )?;
        for mark in ["<", ">"] {
            self.request("nvim_buf_del_mark", vec![Value::from(0), Value::from(mark)])?;
        }
        for register in ["\"", "0", "1", "a"] {
            self.request("nvim_call_function", vec![Value::from("setreg"), Value::Array(vec![Value::from(register), Value::Array(Vec::new())])])?;
        }
        let lines = lines.iter().map(|line| Value::from(*line)).collect();
        self.request("nvim_buf_set_lines", vec![Value::from(0), Value::from(0), Value::from(-1), Value::from(false), Value::Array(lines)])?;
        self.request("nvim_win_set_cursor", vec![Value::from(0), Value::Array(vec![Value::from(1), Value::from(0)])])?;
        self.request("nvim_command", vec![Value::from("setlocal undolevels=1000")])?;
        self.request("nvim_command", vec![Value::from("set nomodified")])?;
        self.request("nvim_input", vec![Value::from("<Esc>")])?;
        Ok(())
    }

    pub fn input(&mut self, keys: &str) -> Result<()> {
        self.request("nvim_input", vec![Value::from(keys)])?;
        // A response-producing call after nvim_input is the synchronization
        // barrier that ensures the input queue has been processed.
        self.request("nvim_eval", vec![Value::from("1")])?;
        Ok(())
    }

    pub fn snapshot(&mut self) -> Result<OracleState> {
        let command = r#"let wren_state = {
              \ 'mode': mode(1),
              \ 'pending_operator': mode(1) =~# '^no' ? mode(1) : '',
              \ 'buffer': getline(1, '$'),
              \ 'cursor': getcurpos()[1:2],
              \ 'selections': {'start': getpos("'<")[1:2], 'end': getpos("'>")[1:2]},
              \ 'registers': map(['"', '0', '1', 'a'], {_, r -> {'name': r, 'value': getreg(r, 1, 1), 'type': getregtype(r)}}),
              \ 'marks': {'local': getmarklist(bufnr()), 'global': getmarklist()},
              \ 'jumplist': getjumplist(),
              \ 'changelist': getchangelist(),
              \ 'search': {'pattern': @/, 'forward': v:searchforward, 'hlsearch': &hlsearch},
              \ 'messages': execute('messages'),
              \ 'undo_tree': undotree(),
              \ 'options': {'tabstop': &l:tabstop, 'shiftwidth': &l:shiftwidth, 'expandtab': &l:expandtab, 'selection': &selection, 'virtualedit': &virtualedit, 'whichwrap': &whichwrap}
              \ }
            echo json_encode(wren_state)"#;
        let response = self.request("nvim_exec2", vec![Value::from(command), Value::Map(vec![(Value::from("output"), Value::from(true))])])?;
        let map = response.as_map().context("nvim_exec2 returned a non-map")?;
        let output = map
            .iter()
            .find_map(|(key, value)| (key.as_str() == Some("output")).then_some(value))
            .and_then(Value::as_str)
            .context("nvim_exec2 omitted output")?;
        let mut state: OracleState = serde_json::from_str(output).context("decode oracle JSON")?;
        strip_volatile_undo_fields(&mut state.undo_tree);
        Ok(state)
    }

    fn request(&mut self, method: &str, parameters: Vec<Value>) -> Result<Value> {
        let message_id = self.next_message_id;
        self.next_message_id = self.next_message_id.saturating_add(1);
        let request = Value::Array(vec![Value::from(0), Value::from(message_id), Value::from(method), Value::Array(parameters)]);
        write_value(&mut self.input, &request).context("encode msgpack-rpc request")?;
        self.input.flush().context("flush msgpack-rpc request")?;

        loop {
            let response = read_value(&mut self.output).context("read msgpack-rpc response")?;
            let Some(parts) = response.as_array() else {
                continue;
            };
            if parts.len() != 4 || parts.first().and_then(Value::as_u64) != Some(1) {
                continue;
            }
            if parts.get(1).and_then(Value::as_u64) != Some(message_id) {
                continue;
            }
            let error = parts.get(2).context("RPC response missing error slot")?;
            if !error.is_nil() {
                bail!("nvim RPC {method} failed: {error:?}");
            }
            return parts.get(3).cloned().context("RPC response missing result");
        }
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_version(api_info: &Value) -> Result<String> {
    let metadata = api_info.as_array().and_then(|values| values.get(1)).and_then(Value::as_map).context("nvim API metadata missing")?;
    let version = metadata
        .iter()
        .find_map(|(key, value)| (key.as_str() == Some("version")).then_some(value))
        .and_then(Value::as_map)
        .context("nvim version metadata missing")?;
    let number = |name: &str| {
        version
            .iter()
            .find_map(|(key, value)| (key.as_str() == Some(name)).then_some(value))
            .and_then(Value::as_u64)
            .with_context(|| format!("nvim version field {name} missing"))
    };
    Ok(format!("{}.{}.{}", number("major")?, number("minor")?, number("patch")?))
}

fn strip_volatile_undo_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("time");
            map.remove("time_cur");
            for child in map.values_mut() {
                strip_volatile_undo_fields(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                strip_volatile_undo_fields(child);
            }
        }
        _ => {}
    }
}
