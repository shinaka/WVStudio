//! The player's side of the reader's pipe: connect to it, check it's really the reader's, read its hello,
//! then serve reads and the quit from one worker thread, so polls and /test don't talk over each other.
//!
//! The worker owns the pipe; `read` and `quit` hand it a request and wait on a channel, so a reader that
//! stops answering is a timeout, not a hang.

use std::io::{BufRead, Read, Write};
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use serde_json::Value as Json;

use super::Readings;
use super::control::Pipe;

/// How long one read waits for the reader to answer (§4.5).
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// The most of one answer that's read: a reading line is well under this.
const MOST: u64 = 1 << 20;

/// The sentence when the reader is from another build (its hello is wrong).
pub(crate) const WRONG_BUILD: &str = "wvsensors.exe is from another build: choose Ask again to update it.";
/// The sentence when the reader never opened its pipe, or stopped answering.
pub(crate) const NO_ANSWER: &str = "PawnIO's sensor reader didn't answer: choose Ask again, or restart the player.";

/// A read or a quit for the worker.
enum Cmd {
    Read,
    Quit,
}

/// The connected reader. One worker thread owns the pipe; this hands it requests.
#[derive(Debug)]
pub struct Client {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    to_worker: Sender<Cmd>,
    answers: Receiver<Result<Readings, String>>,
}

impl Client {
    /// Reads the reader's hello off `reader`, checks it, then starts the worker over `reader`/`writer`.
    /// `Err` is the sentence to show.
    pub fn serve<R, W>(mut reader: R, writer: W) -> Result<Client, String>
    where
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let mut hello = String::new();
        match (&mut reader).take(MOST).read_line(&mut hello) {
            Ok(n) if n > 0 => {}
            _ => return Err(NO_ANSWER.to_string()),
        }
        let ok = serde_json::from_str::<Json>(hello.trim()).ok().is_some_and(|v| v["wvsensors"] == 1);
        if !ok {
            return Err(WRONG_BUILD.to_string());
        }
        let (to_worker, from_player) = channel::<Cmd>();
        let (to_player, answers) = channel::<Result<Readings, String>>();
        std::thread::spawn(move || worker(reader, writer, &from_player, &to_player));
        Ok(Client { inner: Mutex::new(Inner { to_worker, answers }) })
    }
}

impl Pipe for Client {
    fn read(&self) -> Result<Readings, String> {
        let inner = self.inner.lock().unwrap();
        if inner.to_worker.send(Cmd::Read).is_err() {
            return Err(NO_ANSWER.to_string());
        }
        match inner.answers.recv_timeout(READ_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => Err(NO_ANSWER.to_string()),
        }
    }

    fn quit(&self) {
        if let Ok(inner) = self.inner.lock() {
            let _ = inner.to_worker.send(Cmd::Quit);
        }
    }
}

/// The worker: one request at a time over the pipe. A broken pipe ends it, and `read` then times out or
/// sees the channel close.
fn worker<R: BufRead, W: Write>(mut reader: R, mut writer: W, cmds: &Receiver<Cmd>, answers: &Sender<Result<Readings, String>>) {
    for cmd in cmds {
        match cmd {
            Cmd::Read => {
                if writeln!(writer, r#"{{"op":"read"}}"#).and_then(|()| writer.flush()).is_err() {
                    let _ = answers.send(Err(NO_ANSWER.to_string()));
                    return;
                }
                let mut line = String::new();
                match (&mut reader).take(MOST).read_line(&mut line) {
                    Ok(n) if n > 0 => {
                        let readings = serde_json::from_str::<Readings>(line.trim()).map_err(|_| NO_ANSWER.to_string());
                        let _ = answers.send(readings);
                    }
                    _ => {
                        let _ = answers.send(Err(NO_ANSWER.to_string()));
                        return;
                    }
                }
            }
            Cmd::Quit => {
                let _ = writeln!(writer, r#"{{"op":"quit"}}"#);
                let _ = writer.flush();
                return;
            }
        }
    }
}

/// Connects to the reader's pipe for real (never in tests): waits for it, opens it at identification level,
/// checks it belongs to the launched reader, and reads its hello.
#[cfg(not(test))]
pub fn connect_pipe(name: &str, server_pid: u32, timeout: Duration) -> Result<Client, String> {
    use std::io::BufReader;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::{null, null_mut};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};

    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let deadline = Instant::now() + timeout;
    loop {
        unsafe { WaitNamedPipeW(wide.as_ptr(), 500) };
        let handle = unsafe {
            CreateFileW(wide.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING,
                        SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, null_mut())
        };
        if handle != INVALID_HANDLE_VALUE {
            let mut pid = 0u32;
            let same = unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } != 0 && pid == server_pid;
            if !same {
                // not the reader we started: don't talk to it
                unsafe { CloseHandle(handle) };
                return Err(NO_ANSWER.to_string());
            }
            let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
            let reader = match file.try_clone() {
                Ok(clone) => BufReader::new(clone),
                Err(_) => return Err(NO_ANSWER.to_string()),
            };
            return Client::serve(reader, file);
        }
        if Instant::now() >= deadline {
            return Err(NO_ANSWER.to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// What the fake server does with a request.
    enum Reply {
        /// Answer with this line.
        Answer(String),
        /// Read it but never answer, and keep the socket open (the reader hangs).
        Silent,
        /// Close the socket (the reader's pipe breaks).
        Close,
    }

    /// A server on a loopback socket (never 8137/8139/8085) that sends a hello and then does what the test
    /// says to each request.
    fn server(hello: &'static str, reply: impl Fn(&str) -> Reply + Send + 'static) -> (TcpStream, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            let mut writer = socket.try_clone().unwrap();
            if hello.is_empty() {
                return; // close at once, no hello
            }
            let _ = writeln!(writer, "{hello}");
            let _ = writer.flush();
            let mut reader = std::io::BufReader::new(socket);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => match reply(line.trim()) {
                        Reply::Answer(answer) => {
                            if writeln!(writer, "{answer}").and_then(|()| writer.flush()).is_err() {
                                break;
                            }
                        }
                        Reply::Silent => {}
                        Reply::Close => break,
                    },
                    Err(_) => break,
                }
            }
        });
        (TcpStream::connect(addr).unwrap(), thread)
    }

    fn client(stream: TcpStream) -> Result<Client, String> {
        let reader = std::io::BufReader::new(stream.try_clone().unwrap());
        Client::serve(reader, stream)
    }

    /// A good hello, then a read that comes back as readings; then quit.
    #[test]
    fn a_read_round_trip() {
        let readings = Readings { cpu_temp: Some(70.375), notes: vec!["n".into()], ..Readings::default() };
        let json = serde_json::to_string(&readings).unwrap();
        let (stream, thread) =
            server(r#"{"wvsensors":1,"pid":5}"#, move |line| if line == r#"{"op":"read"}"# { Reply::Answer(json.clone()) } else { Reply::Close });
        let client = client(stream).unwrap();
        assert_eq!(client.read().unwrap(), readings);
        client.quit();
        thread.join().unwrap();
    }

    /// A reader from another build (its hello is wrong), or no JSON at all, is refused as a wrong build.
    #[test]
    fn a_wrong_hello_is_refused() {
        for hello in [r#"{"wvsensors":2}"#, "not json"] {
            let (stream, thread) = server(hello, |_| Reply::Silent);
            assert_eq!(client(stream).unwrap_err(), WRONG_BUILD);
            drop(thread.join());
        }
    }

    /// A reader that doesn't answer a read times out as "didn't answer".
    #[test]
    fn a_silent_reader_times_out() {
        let (stream, _thread) = server(r#"{"wvsensors":1}"#, |_| Reply::Silent);
        let client = client(stream).unwrap();
        let start = std::time::Instant::now();
        assert_eq!(client.read().unwrap_err(), NO_ANSWER);
        assert!(start.elapsed() >= READ_TIMEOUT && start.elapsed() < READ_TIMEOUT + Duration::from_secs(2), "{:?}", start.elapsed());
    }

    /// A reader whose pipe breaks is "didn't answer".
    #[test]
    fn a_broken_reader_is_an_error() {
        let (stream, thread) = server(r#"{"wvsensors":1}"#, |_| Reply::Close);
        let client = client(stream).unwrap();
        assert_eq!(client.read().unwrap_err(), NO_ANSWER);
        thread.join().unwrap();
    }

    /// A hello that never comes is "didn't answer".
    #[test]
    fn no_hello_is_an_error() {
        let (stream, thread) = server("", |_| Reply::Silent);
        assert_eq!(client(stream).unwrap_err(), NO_ANSWER);
        thread.join().unwrap();
    }
}
