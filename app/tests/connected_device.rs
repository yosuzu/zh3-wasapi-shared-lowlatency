/// Opt-in hardware check. Reads public properties of the connected ZH3 only.
#[test]
#[ignore = "requires a connected ZH3 and its Fosi driver"]
fn inspect_connected_zh3_without_opening_a_stream() {
    let status = zh3_control::controller::inspect();
    println!("{status}");
    assert_eq!(status["state"], "connected", "{status}");
    assert!(status["profile"]["can_write"].is_boolean(), "{status}");
    if status["profile"]["can_write"] == true {
        assert!(
            !status["snapshot"]["device"]["stream"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert!(!status["target_token"].as_str().unwrap().is_empty());
    }
    assert!(status["snapshot"]["mix"]["rate"].as_u64().unwrap() > 0);
    assert!(
        !status["snapshot"]["device"]["endpoint_id"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}
