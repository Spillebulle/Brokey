//! Privilege through `pkexec`, with the helper's standard streams piped.
//!
//! This is the only place in the workspace that spawns `pkexec`.

use super::Elevated;
use crate::transaction::runner::stream_lines;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub struct Inner {
    child: Child,
}

impl Inner {
    pub fn wait(&mut self) -> std::io::Result<Option<i32>> {
        self.child.wait().map(|status| status.code())
    }
}

pub fn start(helper: &Path, wrapper: &[String]) -> std::io::Result<Elevated> {
    let Some((program, leading)) = wrapper.split_first() else {
        return Err(std::io::Error::other(
            "No privilege wrapper is configured, so nothing can be run as root.",
        ));
    };
    let mut child = Command::new(program)
        .args(leading)
        .arg(helper)
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let input: Option<Box<dyn Write + Send>> = child
        .stdin
        .take()
        .map(|stdin| Box::new(stdin) as Box<dyn Write + Send>);
    let lines = stream_lines(&mut child);
    Ok(Elevated {
        input,
        lines,
        inner: Inner { child },
    })
}
