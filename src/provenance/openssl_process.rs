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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "provenance::openssl_process::tests::process_fixture",
                "--nocapture",
            ])
            .env("MZPEAK_TRUST_PROCESS_FIXTURE", mode);
        command
    }

    #[test]
    fn process_fixture() {
        let Ok(mode) = std::env::var("MZPEAK_TRUST_PROCESS_FIXTURE") else {
            return;
        };
        match mode.as_str() {
            "stall" => {
                fs::write(
                    std::env::var_os("MZPEAK_TRUST_PROCESS_READY").unwrap(),
                    b"ready",
                )
                .unwrap();
                std::thread::sleep(Duration::from_secs(10));
            }
            "large" => {
                std::io::stdout()
                    .write_all(&vec![b'x'; MAX_OUTPUT as usize + 1])
                    .unwrap();
                std::io::stderr()
                    .write_all(&vec![b'x'; MAX_OUTPUT as usize + 1])
                    .unwrap();
            }
            "success" => println!("backend-output"),
            _ => panic!("unknown fixture mode"),
        }
    }

    #[test]
    fn timeout_terminates_and_reaps_child_without_exposing_command() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let stdout = tempfile::tempfile().unwrap();
        let stderr = tempfile::tempfile().unwrap();
        let mut child = fixture("stall")
            .env("MZPEAK_TRUST_PROCESS_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("process fixture did not start");
        }
        let start = Instant::now();
        let error = wait(&mut child, &stdout, &stderr, Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "OpenSSL operation timed out");
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!child.try_wait().unwrap().unwrap().success());
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{}", child.id())).exists());
    }

    #[test]
    fn output_is_bounded_and_start_errors_are_redacted() {
        let output = run(&mut fixture("success")).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("backend-output"));
        assert_eq!(
            run(&mut fixture("large")).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let error = run(Command::new("/private/provider/missing-openssl").arg("private-config"))
            .unwrap_err();
        assert_eq!(error.to_string(), "OpenSSL backend could not start");
    }
}
