#[test]
fn test_command_queue_and_result_parsing() {
    let request_line = "GET /browser/command/result/br_12345 HTTP/1.1";
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let path = parts.get(1).copied().unwrap_or("");
    let request_id = path
        .strip_prefix("/browser/command/result/")
        .unwrap_or(path)
        .to_owned();
    assert_eq!(request_id, "br_12345");
}
