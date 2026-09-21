use serde_json::json;
use std::net::TcpStream;
use std::io::{Write, Read};

fn main() {
    // 1. Test the command queue flow
    println!("Testing command queue flow...");
    let mut stream = TcpStream::connect("127.0.0.1:7317").expect("Daemon must be running on 7317");
    
    // Submit a debugger attach command
    let attach_req = json!({
        "method": "POST /browser/debugger/attach ",
        "body": json!({ "target_id": "test-target-123" })
    });
    
    // Since we are simulating HTTP via raw socket to the daemon's HTTP server
    let http_req = format!(
        "POST /browser/debugger/attach HTTP/1.1\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n\
         {}",
        serde_json::to_string(&json!({"target_id": "test-target-123"})).unwrap().len(),
        serde_json::to_string(&json!({"target_id": "test-target-123"})).unwrap()
    );
    
    stream.write_all(http_req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.contains("202 Accepted"));
    
    println!("Command queue flow verified.");
}
