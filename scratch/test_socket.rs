use std::os::unix::net::{UnixListener, UnixStream};
use std::fs;

fn main() {
    let path = "/tmp/test_mac_socket.sock";
    let _ = fs::remove_file(path);

    println!("Binding...");
    let listener = UnixListener::bind(path).unwrap();
    println!("Dropping listener...");
    drop(listener);

    println!("File exists: {}", fs::metadata(path).is_ok());

    match UnixStream::connect(path) {
        Ok(_) => println!("Connect succeeded! (Unexpected for dropped listener)"),
        Err(e) => println!("Connect failed as expected: {:?}", e),
    }

    let _ = fs::remove_file(path);
}
