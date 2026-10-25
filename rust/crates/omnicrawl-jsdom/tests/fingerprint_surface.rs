//! Canvas / WebGL / 权限 / 属性描述符的伪装能力回归。
//!
//! 这几项的共同点是「探测器会交叉核对」：只看单个返回值是否像真的不够，
//! `permissions` 报授权但 API 不存在、`screen.width` 可写，都会立刻暴露。

use omnicrawl_jsdom::run_inline_scripts;

/// 执行探针并把结果写进 `<div id="out">`，支持不返回值的脚本（Promise 回调）。
fn probe(script: &str) -> String {
    let html = format!(
        r#"<html><body><div id="out">pending</div><script>
        try {{
            {script}
        }} catch (error) {{
            document.getElementById('out').textContent = 'ERR:' + error;
        }}
        </script></body></html>"#
    );
    let outcome = run_inline_scripts(&html);
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    outcome.text.trim().to_string()
}

/// 求一个表达式的值（会自动 `String()`）。
fn value(expression: &str) -> String {
    let script = format!("document.getElementById('out').textContent = String({expression});");
    probe(&script)
}

#[test]
fn canvas_element_exposes_real_context() {
    assert_eq!(
        value("typeof document.createElement('canvas').getContext('2d')"),
        "object"
    );
    // 默认尺寸是规范里的 300×150，不是 0。
    assert_eq!(
        value("(function () { var c = document.createElement('canvas'); return c.width + 'x' + c.height; })()"),
        "300x150"
    );
    // 未知类型返回 null 而不是抛错。
    assert_eq!(
        value("document.createElement('canvas').getContext('nonsense')"),
        "null"
    );
}

#[test]
fn canvas_draws_actual_pixels() {
    // 纯红填充后中心像素必须是红色：这条区分「真画了」与「返回恒定位图」。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 10; c.height = 10;
               var g = c.getContext('2d'); g.fillStyle = '#ff0000'; g.fillRect(0, 0, 10, 10);
               var d = g.getImageData(0, 0, 1, 1);
               document.getElementById('out').textContent = Array.prototype.join.call(d.data, ',');"#
        ),
        "255,0,0,255"
    );
    // 没画到的位置必须保持透明，否则「整张位图被填满」本身就是破绽。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 10; c.height = 10;
               var g = c.getContext('2d'); g.fillRect(0, 0, 5, 5);
               var d = g.getImageData(8, 8, 1, 1);
               document.getElementById('out').textContent = Array.prototype.join.call(d.data, ',');"#
        ),
        "0,0,0,0"
    );
    // alpha 通道要真的参与混合，不能一律 255。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 4; c.height = 4;
               var g = c.getContext('2d'); g.fillStyle = 'rgba(0,255,0,0.5)'; g.fillRect(0, 0, 4, 4);
               document.getElementById('out').textContent =
                 Array.prototype.join.call(g.getImageData(0, 0, 1, 1).data, ',');"#
        ),
        "0,255,0,128"
    );
}

#[test]
fn canvas_paths_and_gradients_produce_gradients() {
    // 圆形填充：中心着色、角落留空。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 20; c.height = 20;
               var g = c.getContext('2d'); g.fillStyle = '#00f';
               g.beginPath(); g.arc(10, 10, 8, 0, Math.PI * 2); g.fill();
               var center = g.getImageData(10, 10, 1, 1).data[2];
               var corner = g.getImageData(0, 0, 1, 1).data[3];
               document.getElementById('out').textContent = center + '|' + corner;"#
        ),
        "255|0"
    );
    // 线性渐变：左端接近黑、右端接近白。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 10; c.height = 10;
               var g = c.getContext('2d');
               var grad = g.createLinearGradient(0, 0, 10, 0);
               grad.addColorStop(0, '#000'); grad.addColorStop(1, '#fff');
               g.fillStyle = grad; g.fillRect(0, 0, 10, 10);
               var left = g.getImageData(0, 0, 1, 1).data[0];
               var right = g.getImageData(9, 0, 1, 1).data[0];
               document.getElementById('out').textContent = (left < 40) + '|' + (right > 210);"#
        ),
        "true|true"
    );
}

/// 字体只在 Windows 上按系统字体文件解析（`resolve_font` 的既定范围），
/// 因此字形光栅化断言只在 Windows 跑；其它平台没有可用的字体文件。
#[cfg(windows)]
#[test]
fn canvas_text_is_rasterized() {
    let inked = probe(
        r#"var c = document.createElement('canvas'); c.width = 80; c.height = 30;
           var g = c.getContext('2d');
           g.font = '20px Arial'; g.fillStyle = '#000'; g.fillText('A', 2, 22);
           var d = g.getImageData(0, 0, 80, 30); var count = 0;
           for (var i = 3; i < d.data.length; i += 4) { if (d.data[i] > 0) count++; }
           document.getElementById('out').textContent = String(count > 20);"#,
    );
    assert_eq!(inked, "true", "fillText 必须真的产生字形像素");
    // measureText 要给出与字号同量级的宽度，不能是 0。
    let width = probe(
        r#"var g = document.createElement('canvas').getContext('2d');
           g.font = '16px Arial';
           document.getElementById('out').textContent = String(g.measureText('Hello').width > 5);"#,
    );
    assert_eq!(width, "true");
}

#[test]
fn canvas_to_data_url_is_a_real_png() {
    // 不同内容必须编码出不同 PNG：恒定位图能过「稳定」校验，过不了这一条。
    assert_eq!(
        probe(
            r#"var a = document.createElement('canvas'); a.width = 20; a.height = 20; a.getContext('2d');
               var b = document.createElement('canvas'); b.width = 20; b.height = 20;
               b.getContext('2d').fillStyle = '#000';
               b.getContext('2d').fillRect(0, 0, 20, 20);
               var ua = a.toDataURL(), ub = b.toDataURL();
               document.getElementById('out').textContent =
                 (ua.indexOf('data:image/png;base64,') === 0) + '|' + (ua === ub);"#
        ),
        "true|false"
    );
    // 同一画布反复取必须稳定（指纹的稳定性要求）。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 8; c.height = 8;
               var g = c.getContext('2d'); g.fillStyle = '#f00'; g.fillRect(0, 0, 8, 8);
               document.getElementById('out').textContent = String(c.toDataURL() === c.toDataURL());"#
        ),
        "true"
    );
}

#[test]
fn canvas_resize_clears_pixels() {
    // `canvas.width = n` 即使赋同值也要清空，与规范一致。
    assert_eq!(
        probe(
            r#"var c = document.createElement('canvas'); c.width = 10; c.height = 10;
               var g = c.getContext('2d'); g.fillStyle = '#f00'; g.fillRect(0, 0, 10, 10);
               var before = g.getImageData(0, 0, 1, 1).data[3];
               c.width = 10;
               var after = c.getContext('2d').getImageData(0, 0, 1, 1).data[3];
               document.getElementById('out').textContent = before + '|' + after;"#
        ),
        "255|0"
    );
}

#[test]
fn canvas_context_is_stable_per_element() {
    assert_eq!(
        value(
            "(function () { var c = document.createElement('canvas'); return c.getContext('2d') === c.getContext('2d'); })()"
        ),
        "true"
    );
    // 两个元素的位图互不干扰。
    assert_eq!(
        probe(
            r#"var a = document.createElement('canvas'); a.width = 4; a.height = 4;
               var b = document.createElement('canvas'); b.width = 4; b.height = 4;
               var ga = a.getContext('2d'), gb = b.getContext('2d');
               gb.fillStyle = '#0f0'; gb.fillRect(0, 0, 4, 4);
               document.getElementById('out').textContent =
                 ga.getImageData(0, 0, 1, 1).data[3] + '|' + gb.getImageData(0, 0, 1, 1).data[3];"#
        ),
        "0|255"
    );
}

#[test]
fn webgl_surface_matches_angle() {
    assert_eq!(value("typeof WebGL2RenderingContext"), "function");
    assert_eq!(
        value("document.createElement('canvas').getContext('webgl') === null"),
        "false"
    );
    assert_eq!(
        value(
            "document.createElement('canvas').getContext('webgl').getParameter(0x9246).indexOf('ANGLE')"
        ),
        "0"
    );
    // 精度格式是设备特征里最常被读的字段。
    assert_eq!(
        probe(
            r#"var gl = document.createElement('canvas').getContext('webgl');
               var p = gl.getShaderPrecisionFormat(0x8B31, 0x8DFB);
               document.getElementById('out').textContent = p.rangeMin + '/' + p.rangeMax + '/' + p.precision;"#
        ),
        "127/127/23"
    );
    assert_eq!(
        probe(
            r#"var gl = document.createElement('canvas').getContext('webgl');
               var a = gl.getContextAttributes();
               document.getElementById('out').textContent = a.antialias + '|' + a.alpha;"#
        ),
        "true|true"
    );
    // 压缩纹理格式不能是 null（真实上下文一定返回数组）。
    assert_eq!(
        value(
            "document.createElement('canvas').getContext('webgl').getParameter(0x86A3).length > 0"
        ),
        "true"
    );
    // 不支持的扩展必须返回 null，否则能力探测会得到错误结论。
    assert_eq!(
        value(
            "document.createElement('canvas').getContext('webgl').getExtension('NOT_A_REAL_EXT')"
        ),
        "null"
    );
}

#[test]
fn permissions_agree_with_exposed_apis() {
    // 读某个权限的 state。
    let state = |name: &str| {
        probe(&format!(
            "navigator.permissions.query({{name:'{name}'}}).then(function (r) {{
                document.getElementById('out').textContent = r.state;
            }}, function () {{
                document.getElementById('out').textContent = 'rejected';
            }});"
        ))
    };
    // 沙箱里没有 mediaDevices，摄像头不可能报已授权。
    assert_eq!(state("camera"), "denied");
    assert_eq!(value("typeof navigator.mediaDevices"), "undefined");
    // 没有 geolocation 接口，就不该报 granted。
    assert_eq!(state("geolocation"), "denied");
    assert_eq!(value("typeof navigator.geolocation"), "undefined");
    // 未申请过的通知是 default。
    assert_eq!(state("notifications"), "default");
    // 规范外的名字必须 reject，而不是给个 granted。
    assert_eq!(state("nonsense"), "rejected");
}

#[test]
fn plain_object_has_fingerprints_removed_from_writable() {
    // `screen` 的字段是只读访问器：描述符里没有 writable，只有 getter。
    assert_eq!(
        probe(
            r#"var d = Object.getOwnPropertyDescriptor(screen, 'width');
               document.getElementById('out').textContent =
                 d.writable + '|' + (typeof d.get) + '|' + (typeof d.set);"#
        ),
        "undefined|function|undefined"
    );
    // 赋值必须失败，取值保持不变。
    assert_eq!(
        probe(
            r#"screen.width = 999;
               document.getElementById('out').textContent = String(screen.width !== 999);"#
        ),
        "true"
    );
    // navigator 的关键字段同样是只读访问器。
    assert_eq!(
        probe(
            r#"var d = Object.getOwnPropertyDescriptor(navigator, 'userAgent');
               document.getElementById('out').textContent = (typeof d.get) + '|' + (d.writable + '');"#
        ),
        "function|undefined"
    );
    assert_eq!(
        probe(
            r#"navigator.userAgent = 'hacked';
               document.getElementById('out').textContent = String(navigator.userAgent.indexOf('hacked') === -1);"#
        ),
        "true"
    );
    assert_eq!(
        probe(
            r#"navigator.webdriver = true;
               document.getElementById('out').textContent = String(navigator.webdriver);"#
        ),
        "false"
    );
    // getter 必须仍报原生形态，否则一眼看出是 JS 补丁。
    assert_eq!(
        probe(
            r#"document.getElementById('out').textContent =
                 Object.getOwnPropertyDescriptor(screen, 'width').get.toString();"#
        ),
        "function () { [native code] }"
    );
}

#[test]
fn screen_geometry_stays_self_consistent() {
    // 这组关系在真机上恒成立，也是「窗口被伪造」最常见的破绽。
    assert_eq!(
        probe(
            r#"document.getElementById('out').textContent =
                 (screen.availHeight < screen.height) + '|' +
                 (innerWidth <= screen.width) + '|' +
                 (innerHeight < outerHeight) + '|' +
                 (screen.colorDepth === screen.pixelDepth);"#
        ),
        "true|true|true|true"
    );
}
