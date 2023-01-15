use anyhow::Result;
use std::io::{BufRead, Lines};
use subprocess::Exec;

#[inline]
pub fn lines(cmd: Exec) -> Result<Lines<impl BufRead>> {
    // We usually have a decent amount of RAM nowdays.
    Ok(std::io::BufReader::with_capacity(8 * 1024 * 1024, cmd.stream_stdout()?).lines())
}
