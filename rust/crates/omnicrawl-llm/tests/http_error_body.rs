#[test]
fn body_with_context_length_classifies() {
    let error = omnicrawl_llm::http_status_error_with_body(
        400,
        "{\"error\":{\"message\":\"This model's maximum context length is 8192 tokens\"}}",
    );
    assert_eq!(
        error.message,
        omnicrawl_llm::CONTEXT_LENGTH_EXCEEDED_MESSAGE,
        "分类结果：{error:?}"
    );
}

#[test]
fn empty_body_falls_back_to_status_ladder() {
    let error = omnicrawl_llm::http_status_error_with_body(400, "");
    assert!(
        error.status_code == Some(400) || error.message.contains("400"),
        "空正文也要给出状态码线索：{error:?}"
    );
}
