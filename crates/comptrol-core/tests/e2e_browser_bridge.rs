use serde_json::json;

#[test]
fn test_browser_bridge_round_trip_path_and_payload() {
    let request_line = "GET /browser/command/result/br_12345 HTTP/1.1";
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let path = parts.get(1).copied().unwrap_or("");
    let request_id = path
        .strip_prefix("/browser/command/result/")
        .unwrap_or(path)
        .to_owned();
    assert_eq!(request_id, "br_12345");

    let error_val = Some(json!(null));
    let has_error = error_val.as_ref().filter(|v| !v.is_null()).is_some();
    assert!(!has_error, "null error should be treated as no error");
}
