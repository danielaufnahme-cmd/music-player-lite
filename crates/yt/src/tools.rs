//! Finding and running the external `yt-dlp` and `ffmpeg` binaries.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::net::{Error, Result};

fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
}

fn next_to_exe(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.join(name);
    p.is_file().then_some(p)
}

/// Env var (e.g. GEONIX_YTDLP), then PATH, then next to the running executable.
pub fn locate(env_var: &str, base: &str) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(env_var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    let name = exe_name(base);
    on_path(&name).or_else(|| next_to_exe(&name))
}

pub fn command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console window pops up for yt-dlp / ffmpeg.
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

pub struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Run a tool to completion, capturing output.
pub async fn run(program: &Path, args: &[String], timeout: Duration) -> Result<Output> {
    let mut cmd = command(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| Error::Msg(format!("could not start {}: {e}", program.display())))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| Error::Net(format!("{} timed out", program.display())))?
        .map_err(|e| Error::Msg(format!("{} failed: {e}", program.display())))?;
    Ok(Output { success: out.status.success(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
}

/// Pipe `input` through a tool and return its stdout.
pub async fn pipe(program: &Path, args: &[&str], input: Vec<u8>, timeout: Duration) -> Result<Vec<u8>> {
    let mut cmd = command(program);
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Error::Msg(format!("could not start {}: {e}", program.display())))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&input).await;
        drop(stdin);
    });
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| Error::Net(format!("{} timed out", program.display())))?
        .map_err(|e| Error::Msg(e.to_string()))?;
    let _ = writer.await;
    if !out.status.success() {
        return Err(Error::Msg(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(out.stdout)
}

/// Extra yt-dlp options so YouTube's JavaScript challenges get solved: allow
/// fetching the EJS solver scripts when they aren't bundled or installed, and
/// use Node when Deno (yt-dlp's default runtime) is missing. Without a solved
/// challenge yt-dlp is slow and formats go missing. yt-dlp builds that don't
/// know these options get none. Probed once per process.
pub async fn ytdlp_js_args(ytdlp: &Path) -> Vec<String> {
    static ARGS: tokio::sync::OnceCell<Vec<String>> = tokio::sync::OnceCell::const_new();
    ARGS.get_or_init(|| async {
        match run(ytdlp, &["--help".to_string()], Duration::from_secs(20)).await {
            Ok(out) if out.success => js_args_from_help(
                &String::from_utf8_lossy(&out.stdout),
                on_path(&exe_name("deno")).is_some(),
                on_path(&exe_name("node")).is_some(),
            ),
            _ => Vec::new(),
        }
    })
    .await
    .clone()
}

/// The JS-challenge options for a yt-dlp whose `--help` is `help`.
pub fn js_args_from_help(help: &str, has_deno: bool, has_node: bool) -> Vec<String> {
    let mut args = Vec::new();
    if help.contains("--remote-components") {
        args.extend(["--remote-components".to_string(), "ejs:github".to_string()]);
    }
    if help.contains("--js-runtimes") && !has_deno && has_node {
        args.extend(["--js-runtimes".to_string(), "node".to_string()]);
    }
    args
}

/// The first `ERROR:` line yt-dlp printed, or the last non-empty stderr line.
pub fn ytdlp_error(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .map(|l| l.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_args_only_for_yt_dlp_that_knows_them() {
        let new = "  --js-runtimes RUNTIME[:PATH]\n  --remote-components COMPONENT\n";
        assert_eq!(js_args_from_help(new, true, true), ["--remote-components", "ejs:github"]);
        assert_eq!(js_args_from_help(new, false, true), ["--remote-components", "ejs:github", "--js-runtimes", "node"]);
        assert_eq!(js_args_from_help(new, false, false), ["--remote-components", "ejs:github"]);
        assert!(js_args_from_help("  --flat-playlist\n", false, true).is_empty());
    }
}
