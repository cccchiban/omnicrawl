use omnicrawl_jsdom::{run_inline_scripts, touches_dom};

#[test]
fn inner_html_written_by_script_lands_in_text() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="app"></div>
        <script>document.getElementById('app').innerHTML = '<p>铁钉草是豆科植物</p>';</script>
        </body></html>"#,
    );
    assert_eq!(outcome.scripts, 1);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.touched);
    assert!(
        outcome.text.contains("铁钉草是豆科植物"),
        "正文应包含脚本写入的内容，实际：{}",
        outcome.text
    );
    assert!(touches_dom(&outcome));
}

#[test]
fn text_content_and_create_element_work() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="root"></div><script>
        var p = document.createElement('p');
        p.textContent = '由脚本创建的段落';
        document.getElementById('root').appendChild(p);
        document.title;
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.text.contains("由脚本创建的段落"),
        "{}",
        outcome.text
    );
    // 序列化后的 HTML 里应当出现真实元素。
    assert!(outcome.html.contains("<p>"), "{}", outcome.html);
}

#[test]
fn document_write_is_captured() {
    let outcome = run_inline_scripts(
        r#"<html><body><script>document.write('<h1>写出的小标题</h1>');</script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.written.contains("写出的小标题"),
        "{}",
        outcome.written
    );
    assert!(outcome.text.contains("写出的小标题"), "{}", outcome.text);
}

#[test]
fn set_attribute_and_selector_matching() {
    let outcome = run_inline_scripts(
        r#"<html><body><div class="card featured" id="c1">原文本</div><script>
        var card = document.querySelector('.card.featured');
        card.setAttribute('data-state', 'ready');
        document.querySelector('#c1').textContent = '改过的文本';
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.html.contains("data-state=\"ready\""),
        "{}",
        outcome.html
    );
    assert!(outcome.text.contains("改过的文本"), "{}", outcome.text);
    assert!(!outcome.text.contains("原文本"), "{}", outcome.text);
}

#[test]
fn query_selector_all_returns_every_match() {
    let outcome = run_inline_scripts(
        r#"<html><body><ul><li>一</li><li>二</li><li>三</li></ul><script>
        var items = document.querySelectorAll('li');
        window.__count = items.length;
        document.querySelector('ul').setAttribute('data-count', String(items.length));
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.html.contains("data-count=\"3\""),
        "应匹配到三个 li：{}",
        outcome.html
    );
}

#[test]
fn external_scripts_and_data_blocks_are_not_executed() {
    let outcome = run_inline_scripts(
        r#"<html><body>
        <script src="/bundle.js">document.body.textContent = '不该执行';</script>
        <script type="application/ld+json">{"a":1}</script>
        <script type="text/template"><div>模板</div></script>
        </body></html>"#,
    );
    assert_eq!(outcome.scripts, 0, "外链与数据块都不算内联脚本");
    assert!(!outcome.touched);
    assert!(!outcome.text.contains("不该执行"), "{}", outcome.text);
}

#[test]
fn infinite_loop_is_stopped_by_iteration_limit() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="a"></div><script>
        document.getElementById('a').textContent = '先写一句';
        while (true) {}
        </script></body></html>"#,
    );
    // 限流生效：脚本被中断并记录错误，但进程不挂死、已写入的内容保留。
    assert!(!outcome.errors.is_empty(), "死循环应当被中断");
    assert!(outcome.text.contains("先写一句"), "{}", outcome.text);
}

#[test]
fn script_error_does_not_block_following_scripts() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="a"></div>
        <script>throw new Error('第一个脚本失败');</script>
        <script>document.getElementById('a').textContent = '第二个脚本照常执行';</script>
        </body></html>"#,
    );
    assert_eq!(outcome.errors.len(), 1);
    assert!(
        outcome.text.contains("第二个脚本照常执行"),
        "{}",
        outcome.text
    );
}

#[test]
fn sandbox_has_no_network_or_timers() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="a"></div><script>
        var reached = [];
        // 网络 API 存在但调用即失败（见 fingerprint 测试）：沙箱自己不联网。
        if (typeof fetch === 'function') reached.push('fetch-stub');
        if (typeof setTimeout === 'function') reached.push('timer-shape');
        // 存储是内存空实现：形状与浏览器一致，但不落盘、不跨请求复用。
        if (typeof localStorage === 'object') reached.push('storage-shape');
        document.getElementById('a').textContent = reached.join(',');
        </script></body></html>"#,
    );
    assert!(
        outcome
            .text
            .contains("fetch-stub,timer-shape,storage-shape"),
        "沙箱的形状应齐全且不真正联网：{}",
        outcome.text
    );
}

#[test]
fn plain_page_reports_no_touch() {
    let outcome = run_inline_scripts("<html><body><p>纯静态正文</p></body></html>");
    assert_eq!(outcome.scripts, 0);
    assert!(!outcome.touched);
    assert!(
        !touches_dom(&outcome),
        "没有脚本动过 DOM 时不应判为 JS 拼装页"
    );
    assert!(outcome.text.contains("纯静态正文"));
}

#[test]
fn missing_body_is_synthesised() {
    let outcome = run_inline_scripts(
        "<div id=app></div><script>document.getElementById('app').textContent='片段页';</script>",
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(outcome.text.contains("片段页"), "{}", outcome.text);
}

/// React 的协调器只有 `appendChild` 是挂不上一棵树的：它依赖 `insertBefore`
/// 落位、`childNodes` 遍历、`removeChild` 卸载。这组成员缺失时页面不报错，
/// `#root` 却永远空着——正是「SPA 重建不出正文」的典型成因。
#[test]
fn react_host_config_primitives_are_available() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="root"></div><script>
        var root = document.getElementById('root');
        var a = document.createElement('span');
        a.textContent = '第一段';
        var b = document.createElement('span');
        b.textContent = '第二段';
        root.appendChild(a);
        root.insertBefore(b, a);
        var report = [
          root.childNodes.length,
          root.firstChild.textContent,
          root.firstChild.nextSibling.textContent,
          root.lastChild.textContent,
          root.firstChild.parentNode === root,
          root.contains(a),
          root.contains(root),
        ].join('|');
        root.removeChild(b);
        document.body.setAttribute('data-report',
          report + '|after=' + root.childNodes.length);
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome
            .html
            .contains("data-report=\"2|第二段|第一段|第一段|true|true|true|after=1\""),
        "树操作结果不符合预期：{}",
        outcome.html
    );
}

/// 文本节点必须是 `nodeType === 3`：React 靠它区分「元素」与「文本」，
/// 一律返回 1 会让 `createTextInstance` 之后立刻崩。
#[test]
fn text_nodes_report_their_own_node_type() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="root"></div><script>
        var el = document.createElement('p');
        var text = document.createTextNode('文字');
        document.getElementById('root').appendChild(el);
        el.appendChild(text);
        document.body.setAttribute('data-types',
          el.nodeType + '/' + text.nodeType + '/' + document.nodeType
          + '/' + text.nodeValue);
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.html.contains("data-types=\"1/3/9/文字\""),
        "nodeType 不正确：{}",
        outcome.html
    );
}

/// `postMessage` 必须像宏任务一样排队：一旦同步递归派发，React 调度器
/// 的自续链会耗尽解释器递归上限（实测 170 层）后静默失效。
#[test]
fn message_channel_dispatch_survives_many_rounds() {
    let outcome = run_inline_scripts(
        r#"<html><body><script>
        var work = 0;
        var channel = new MessageChannel();
        channel.port1.onmessage = function(){
          work++;
          if (work < 300) { channel.port2.postMessage(null); }
          else { document.body.setAttribute('data-work', 'rounds=' + work); }
        };
        channel.port2.postMessage(null);
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.html.contains("data-work=\"rounds=300\""),
        "消息自续链没有跑满：{}",
        outcome.html
    );
}

/// 模板克隆与整批替换是 SPA 渲染的常规写法。
#[test]
fn clone_node_and_replace_children_work() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="root"></div><script>
        var host = document.getElementById('root');
        var source = document.createElement('div');
        source.innerHTML = '<em>克隆内容</em>';
        host.appendChild(source.cloneNode(true));
        var copy = host.firstChild.cloneNode(true);
        host.replaceChildren(copy);
        document.body.setAttribute('data-out', host.innerHTML.length > 0 ? 'ok' : 'empty');
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.text.contains("克隆内容"),
        "克隆后的文本应进入正文：{}",
        outcome.text
    );
    assert!(outcome.html.contains("data-out=\"ok\""), "{}", outcome.html);
}

/// 布局库读 `getBoundingClientRect().height` 做偏移计算，`undefined` 会在
/// `Math.ceil` 上抛错并让整棵子树被错误边界替换。
#[test]
fn bounding_rect_returns_numeric_fields() {
    let outcome = run_inline_scripts(
        r#"<html><body><div id="root"></div><script>
        var rect = document.getElementById('root').getBoundingClientRect();
        document.body.setAttribute('data-rect',
          Math.ceil(rect.height + 16) + '/' + rect.top + '/' + rect.width);
        </script></body></html>"#,
    );
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    assert!(
        outcome.html.contains("data-rect=\"16/0/0\""),
        "{}",
        outcome.html
    );
}
