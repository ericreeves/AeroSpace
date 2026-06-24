use std::io::Write;
use std::os::unix::net::UnixStream;

fn main() {
    let event = std::env::args().nth(1).unwrap_or_default();
    if event.is_empty() {
        return;
    }
    // Must match aerospace-helper's socket_path(): /tmp/aerospace-helper-$USER.sock
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
    let socket = format!("/tmp/aerospace-helper-{}.sock", user);
    if let Ok(mut stream) = UnixStream::connect(&socket) {
        let _ = writeln!(stream, "{}", event);
    }
}
