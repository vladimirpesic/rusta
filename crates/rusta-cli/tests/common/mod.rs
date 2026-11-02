//! What the pseudo-terminal tests share: the fixtures and the mock model
//! endpoint, `tests/fixtures/mock_llm.py`.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::Value;

pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The mock endpoint, stopped when dropped.
pub struct Mock {
    child: Child,
    pub port: u16,
    log: PathBuf,
}

impl Mock {
    /// Start the endpoint in `mode` (`text` or `tool`); it logs every request
    /// it receives under `dir`.
    pub fn start(mode: &str, dir: &Path) -> Self {
        let log = dir.join("requests.jsonl");
        let mut child = Command::new("python3")
            .arg("-I")
            .arg(fixture("mock_llm.py"))
            .arg(mode)
            .arg(&log)
            .arg("0.4")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("python3 runs the mock endpoint");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("the mock prints its port");
        let port = line
            .trim()
            .strip_prefix("PORT ")
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in the mock's first line: {line:?}"));
        Self { child, port, log }
    }

    /// Every request received, in order: `{n, messages: [{role, text}]}`.
    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON request record"))
            .collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
