//! 本地 MCP Server 与内置文档的对照测试。

mod common;

use omnicrawl_mcp::bundled::{bundled_doc_names, bundled_doc_uri, read_bundled_doc};
use omnicrawl_mcp::server::LocalMcpServer;
use serde_json::Value;

#[test]
fn server_responses_match_python() {
    let data = common::fixture();
    let root = common::temp_workspace("server-parity");
    common::prepare_workspace(&root);
    let server = LocalMcpServer::new(&root);
    let root_text = root.to_string_lossy().to_string();
    let cases = common::cases(&data, "server");
    assert!(cases.len() >= 20, "数据集太小：{}", cases.len());

    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let request = common::field(&case, "request");
        let actual = match server.handle_message(request.as_object().expect("请求必须是对象"))
        {
            Some(response) => response,
            None => Value::Null,
        };
        let expected = common::substitute(
            common::field(&case, "response"),
            common::WORKSPACE_PLACEHOLDER,
            &root_text,
        );
        assert_eq!(actual, expected, "用例 {name} 的响应不一致");
    }
}

#[test]
fn bundled_documents_match_python_package() {
    let data = common::fixture();
    let names: Vec<String> = common::field(&data, "bundled_docs")
        .get("names")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let hashes = common::field(&data, "bundled_docs")
        .get("hashes")
        .cloned()
        .unwrap_or(Value::Null);

    let embedded = bundled_doc_names();
    assert_eq!(embedded.len(), names.len(), "内置文档数量不一致");
    for (index, name) in names.iter().enumerate() {
        assert_eq!(embedded[index], name, "内置文档顺序不一致");
        let uri = bundled_doc_uri(name).expect("URI 应当合法");
        let text = read_bundled_doc(&uri).expect("文档应当可读");
        assert_eq!(
            common::sha256_hex(text.as_bytes()),
            hashes[name].as_str().unwrap_or_default(),
            "文档 {name} 的内容与安装包不一致"
        );
    }
}
