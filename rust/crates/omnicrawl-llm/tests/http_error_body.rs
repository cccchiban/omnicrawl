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

#[test]
fn gateway_wording_survives_the_mapping() {
    // 真实事故：网关用 400 回「同一请求不能重复提交相同的 call_id」，固定文案把它盖掉了，
    // 用户只能去云端后台才看得到原因。上游原话必须留在错误面里。
    let error = omnicrawl_llm::http_status_error_with_body(
        400,
        "{\"error\":\"同一请求不能重复提交相同的 call_id\",\"type\":\"invalid_tool_state\"}",
    );
    assert!(
        error.message.contains("同一请求不能重复提交相同的 call_id"),
        "上游原话必须保留：{error:?}"
    );
    assert!(error.message.contains("HTTP 400"), "状态码线索也要保留：{error:?}");
}

#[test]
fn html_error_pages_are_not_pasted_into_messages() {
    let error = omnicrawl_llm::http_status_error_with_body(
        502,
        "<html><head><title>502 Bad Gateway</title></head><body>nginx</body></html>",
    );
    assert!(
        !error.message.contains("<html>"),
        "HTML 页面不该进会话：{error:?}"
    );
}
