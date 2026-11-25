//! Bounded, shell-free OpenSSL execution for certificate and timestamp checks.
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    process::{Child, Command, ExitStatus, Output, Stdio},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: u64 = 64 * 1024;
const DIAGNOSTIC_BYTES: u64 = 4096;

fn failure(kind: io::ErrorKind, message: &'static str) -> io::Error {
    // Neither command arguments nor provider output belongs in execution errors.
    io::Error::new(kind, message)
}

fn wait(
    child: &mut Child,
    stdout: &File,
    stderr: &File,
    timeout: Duration,
) -> io::Result<ExitStatus> {
    let started = Instant::now();
    let mut reaped = false;
    let result = (|| loop {
        if stdout.metadata()?.len() > MAX_OUTPUT || stderr.metadata()?.len() > MAX_OUTPUT {
            break Err(failure(
                io::ErrorKind::InvalidData,
                "OpenSSL diagnostic output exceeded its limit",
            ));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break Err(failure(
                io::ErrorKind::TimedOut,
                "OpenSSL operation timed out",
            ));
        }
        match child.try_wait()? {
            Some(status) => {
                reaped = true;
                // Account for output written between the last size check and exit.
                if stdout.metadata()?.len() > MAX_OUTPUT || stderr.metadata()?.len() > MAX_OUTPUT {
                    break Err(failure(
                        io::ErrorKind::InvalidData,
                        "OpenSSL diagnostic output exceeded its limit",
                    ));
                }
                break Ok(status);
            }
            None => std::thread::sleep(remaining.min(Duration::from_millis(10))),
        }
    })();
    if result.is_err() && !reaped {
        // Reap even if the child exits between the status check and kill.
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

pub(super) fn run(command: &mut Command) -> io::Result<Output> {
    run_with_timeout(command, TIMEOUT)
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    // Files avoid pipe-buffer deadlocks while the parent enforces the deadline.
    // Both streams have an operation-level size limit and bounded retained text.
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()
        .map_err(|error| failure(error.kind(), "OpenSSL backend could not start"))?;
    let status = wait(&mut child, &stdout, &stderr, timeout)?;
    fn read(file: &mut File) -> io::Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = vec![];
        file.take(DIAGNOSTIC_BYTES).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    Ok(Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}
