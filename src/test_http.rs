//! Small bounded HTTP fixture shared by native-account unit tests.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
pub(crate) type Requests = Arc<Mutex<Vec<(String, String)>>>;
pub(crate) fn server(
    replies: Vec<(u16, &'static str, String)>,
) -> (String, Requests, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured: Requests = Arc::default();
    let out = captured.clone();
    let task = std::thread::spawn(move || {
        for (status, content_type, body) in replies {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "mock HTTP request timed out"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                assert!(headers.len() < 65_536);
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut request_body = vec![0; length];
            stream.read_exact(&mut request_body).unwrap();
            out.lock()
                .unwrap()
                .push((headers, String::from_utf8(request_body).unwrap()));
            write!(stream,"HTTP/1.1 {status} fixture\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    (base, captured, task)
}
pub(crate) struct AuthHome {
    previous: Option<std::ffi::OsString>,
    pub path: std::path::PathBuf,
}
impl AuthHome {
    pub fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("sui-auth-test-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&path).unwrap();
        let previous = std::env::var_os("SUI_HOME");
        unsafe {
            std::env::set_var("SUI_HOME", &path);
        }
        Self { previous, path }
    }
}
impl Drop for AuthHome {
    fn drop(&mut self) {
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("SUI_HOME", previous);
            } else {
                std::env::remove_var("SUI_HOME");
            }
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
