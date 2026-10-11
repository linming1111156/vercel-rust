use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{mpsc, oneshot};

const READ_CHUNK: usize = 8 * 1024;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 40;

pub enum Backend {
    Pty {
        master: Mutex<Box<dyn MasterPty + Send>>,
        killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    },
    #[cfg(unix)]
    Pipe { pid: u32 },
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 一个交互式终端进程（真实 PTY，或 script/python 降级实现）。
pub struct Terminal {
    /// 写入终端 stdin 的通道。
    pub input: mpsc::Sender<Vec<u8>>,
    /// 终端输出。
    pub output: mpsc::Receiver<Vec<u8>>,
    /// 进程退出码。
    pub exit: oneshot::Receiver<i32>,
    pub backend: Backend,
}

impl Backend {
    pub fn resize(&self, cols: u16, rows: u16) {
        match self {
            Backend::Pty { master, .. } => {
                if let Ok(master) = master.lock() {
                    let _ = master.resize(PtySize {
                        rows: rows.max(1),
                        cols: cols.max(1),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            #[cfg(unix)]
            Backend::Pipe { .. } => {}
        }
    }

    pub fn kill(&self) {
        match self {
            Backend::Pty { killer, .. } => {
                if let Ok(mut killer) = killer.lock() {
                    let _ = killer.kill();
                }
            }
            #[cfg(unix)]
            Backend::Pipe { pid } => {
                let pid = *pid as i32;
                // 先按进程组杀，覆盖 script/python 包装的子 shell。
                unsafe {
                    if libc::kill(-pid, libc::SIGKILL) != 0 {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

fn shell() -> String {
    match std::env::var("SHELL") {
        Ok(value) if !value.is_empty() => value,
        _ => {
            if cfg!(windows) {
                "powershell.exe".to_string()
            } else {
                "bash".to_string()
            }
        }
    }
}

fn working_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// 优先使用 PTY，失败时降级（对应 node-pty + script/python 的分支）。
pub fn spawn() -> Option<Terminal> {
    if let Some(terminal) = spawn_pty() {
        return Some(terminal);
    }
    tracing::warn!("[Terminal] PTY 不可用，使用降级模式");
    spawn_fallback()
}

#[cfg(unix)]
fn spawn_fallback() -> Option<Terminal> {
    spawn_pipe()
}

#[cfg(not(unix))]
fn spawn_fallback() -> Option<Terminal> {
    None
}

fn spawn_pty() -> Option<Terminal> {
    let system = native_pty_system();
    let pair = system
        .openpty(PtySize {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .ok()?;

    let mut builder = CommandBuilder::new(shell());
    builder.env("TERM", "xterm");
    builder.cwd(working_dir());

    let mut child = pair.slave.spawn_command(builder).ok()?;
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().ok()?;
    let writer = pair.master.take_writer().ok()?;
    let killer = child.clone_killer();

    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(64);
    let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(64);
    let (exit_tx, exit_rx) = oneshot::channel::<i32>();

    tokio::task::spawn_blocking(move || read_loop(reader, output_tx));
    tokio::task::spawn_blocking(move || write_loop(writer, input_rx));
    tokio::task::spawn_blocking(move || {
        let code = child
            .wait()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(-1);
        let _ = exit_tx.send(code);
    });

    Some(Terminal {
        input: input_tx,
        output: output_rx,
        exit: exit_rx,
        backend: Backend::Pty {
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
        },
    })
}

/// 降级实现：`script -qfc "$SHELL -i" /dev/null`，再退到 python 的 pty 模块。
#[cfg(unix)]
fn spawn_pipe() -> Option<Terminal> {
    let shell = shell();
    let candidates: [(&str, Vec<String>); 3] = [
        (
            "script",
            vec![
                "-qfc".to_string(),
                format!("{shell} -i"),
                "/dev/null".to_string(),
            ],
        ),
        (
            "python3",
            vec![
                "-c".to_string(),
                "import pty,os,sys;pty.spawn([os.environ.get(\"SHELL\",\"/bin/bash\"),\"-i\"])"
                    .to_string(),
            ],
        ),
        (
            "python",
            vec![
                "-c".to_string(),
                "import pty,os,sys;pty.spawn([os.environ.get(\"SHELL\",\"/bin/bash\"),\"-i\"])"
                    .to_string(),
            ],
        ),
    ];

    for (program, args) in candidates {
        let mut command = Command::new(program);
        command
            .args(&args)
            .current_dir(working_dir())
            .env("TERM", "xterm")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => continue,
        };
        let Some(stdin) = child.stdin.take() else {
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            continue;
        };
        let Some(stderr) = child.stderr.take() else {
            continue;
        };

        let pid = child.id();
        let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(64);
        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>(64);
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();

        let stdout_tx = output_tx.clone();
        tokio::task::spawn_blocking(move || read_loop(Box::new(stdout), stdout_tx));
        tokio::task::spawn_blocking(move || read_loop(Box::new(stderr), output_tx));
        tokio::task::spawn_blocking(move || write_loop(Box::new(stdin), input_rx));
        tokio::task::spawn_blocking(move || {
            let code = child
                .wait()
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
            let _ = exit_tx.send(code);
        });

        tracing::debug!("[Terminal] 降级模式已启动: {program}");
        return Some(Terminal {
            input: input_tx,
            output: output_rx,
            exit: exit_rx,
            backend: Backend::Pipe { pid },
        });
    }

    tracing::error!("[Terminal] 无法创建伪终端（script/python 均不可用）");
    None
}

fn read_loop(mut reader: Box<dyn Read + Send>, output: mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                if output.blocking_send(buffer[..size].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

fn write_loop(mut writer: Box<dyn Write + Send>, mut input: mpsc::Receiver<Vec<u8>>) {
    while let Some(data) = input.blocking_recv() {
        if writer.write_all(&data).is_err() || writer.flush().is_err() {
            break;
        }
    }
}
